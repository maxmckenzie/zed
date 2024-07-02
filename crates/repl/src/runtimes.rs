use anyhow::{Context as _, Result};
use futures::{
    channel::mpsc::{self, Receiver},
    future::Shared,
    stream::{self, SelectAll, StreamExt},
    SinkExt as _,
};
use gpui::{AppContext, EntityId, Task};
use project::Fs;
use runtimelib::{
    dirs, ConnectionInfo, ExecutionState, JupyterKernelspec, JupyterMessage, JupyterMessageContent,
    KernelInfoReply,
};
use smol::{net::TcpListener, process::Command};
use std::{
    fmt::Debug,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
};
use ui::{Color, Indicator};

#[derive(Debug, Clone)]
pub struct RuntimeSpecification {
    pub name: String,
    pub path: PathBuf,
    pub kernelspec: JupyterKernelspec,
}

impl RuntimeSpecification {
    #[must_use]
    fn command(&self, connection_path: &PathBuf) -> anyhow::Result<Command> {
        let argv = &self.kernelspec.argv;

        anyhow::ensure!(!argv.is_empty(), "Empty argv in kernelspec {}", self.name);
        anyhow::ensure!(argv.len() >= 2, "Invalid argv in kernelspec {}", self.name);
        anyhow::ensure!(
            argv.iter().any(|arg| arg == "{connection_file}"),
            "Missing 'connection_file' in argv in kernelspec {}",
            self.name
        );

        let mut cmd = Command::new(&argv[0]);

        for arg in &argv[1..] {
            if arg == "{connection_file}" {
                cmd.arg(connection_path);
            } else {
                cmd.arg(arg);
            }
        }

        if let Some(env) = &self.kernelspec.env {
            cmd.envs(env);
        }

        Ok(cmd)
    }
}

// Find a set of open ports. This creates a listener with port set to 0. The listener will be closed at the end when it goes out of scope.
// There's a race condition between closing the ports and usage by a kernel, but it's inherent to the Jupyter protocol.
async fn peek_ports(ip: IpAddr) -> anyhow::Result<[u16; 5]> {
    let mut addr_zeroport: SocketAddr = SocketAddr::new(ip, 0);
    addr_zeroport.set_port(0);
    let mut ports: [u16; 5] = [0; 5];
    for i in 0..5 {
        let listener = TcpListener::bind(addr_zeroport).await?;
        let addr = listener.local_addr()?;
        ports[i] = addr.port();
    }
    Ok(ports)
}

#[derive(Debug)]
pub enum Kernel {
    RunningKernel(RunningKernel),
    StartingKernel(Shared<Task<()>>),
    ErroredLaunch(String),
    ShuttingDown,
    Shutdown,
}

impl Kernel {
    pub fn dot(&mut self) -> Indicator {
        match self {
            Kernel::RunningKernel(kernel) => match kernel.execution_state {
                ExecutionState::Idle => Indicator::dot().color(Color::Success),
                ExecutionState::Busy => Indicator::dot().color(Color::Modified),
            },
            Kernel::StartingKernel(_) => Indicator::dot().color(Color::Modified),
            Kernel::ErroredLaunch(_) => Indicator::dot().color(Color::Error),
            Kernel::ShuttingDown => Indicator::dot().color(Color::Modified),
            Kernel::Shutdown => Indicator::dot().color(Color::Disabled),
        }
    }

    pub fn set_execution_state(&mut self, status: &ExecutionState) {
        match self {
            Kernel::RunningKernel(running_kernel) => {
                running_kernel.execution_state = status.clone();
            }
            _ => {}
        }
    }

    pub fn set_kernel_info(&mut self, kernel_info: &KernelInfoReply) {
        match self {
            Kernel::RunningKernel(running_kernel) => {
                running_kernel.kernel_info = Some(kernel_info.clone());
            }
            _ => {}
        }
    }
}

pub struct RunningKernel {
    #[allow(unused)]
    pub process: smol::process::Child,
    #[allow(unused)]
    shell_task: Task<anyhow::Result<()>>,
    #[allow(unused)]
    iopub_task: Task<anyhow::Result<()>>,
    #[allow(unused)]
    control_task: Task<anyhow::Result<()>>,
    #[allow(unused)]
    routing_task: Task<anyhow::Result<()>>,
    connection_path: PathBuf,
    pub request_tx: mpsc::Sender<JupyterMessage>,
    pub execution_state: ExecutionState,
    pub kernel_info: Option<KernelInfoReply>,
}

type JupyterMessageChannel = stream::SelectAll<Receiver<JupyterMessage>>;

impl Debug for RunningKernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunningKernel")
            .field("process", &self.process)
            .finish()
    }
}

impl RunningKernel {
    pub fn new(
        runtime_specification: RuntimeSpecification,
        entity_id: EntityId,
        fs: Arc<dyn Fs>,
        cx: &mut AppContext,
    ) -> Task<anyhow::Result<(Self, JupyterMessageChannel)>> {
        cx.spawn(|cx| async move {
            let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
            let ports = peek_ports(ip).await?;

            let connection_info = ConnectionInfo {
                transport: "tcp".to_string(),
                ip: ip.to_string(),
                stdin_port: ports[0],
                control_port: ports[1],
                hb_port: ports[2],
                shell_port: ports[3],
                iopub_port: ports[4],
                signature_scheme: "hmac-sha256".to_string(),
                key: uuid::Uuid::new_v4().to_string(),
                kernel_name: Some(format!("zed-{}", runtime_specification.name)),
            };

            let connection_path = dirs::runtime_dir().join(format!("kernel-zed-{entity_id}.json"));
            let content = serde_json::to_string(&connection_info)?;
            // write out file to disk for kernel
            fs.atomic_write(connection_path.clone(), content).await?;

            let mut cmd = runtime_specification.command(&connection_path)?;
            let process = cmd
                // .stdout(Stdio::null())
                // .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .context("failed to start the kernel process")?;

            let mut iopub_socket = connection_info.create_client_iopub_connection("").await?;
            let mut shell_socket = connection_info.create_client_shell_connection().await?;
            let mut control_socket = connection_info.create_client_control_connection().await?;

            let (mut iopub, iosub) = futures::channel::mpsc::channel(100);

            let (request_tx, mut request_rx) =
                futures::channel::mpsc::channel::<JupyterMessage>(100);

            let (mut control_reply_tx, control_reply_rx) = futures::channel::mpsc::channel(100);
            let (mut shell_reply_tx, shell_reply_rx) = futures::channel::mpsc::channel(100);

            let mut messages_rx = SelectAll::new();
            messages_rx.push(iosub);
            messages_rx.push(control_reply_rx);
            messages_rx.push(shell_reply_rx);

            let iopub_task = cx.background_executor().spawn({
                async move {
                    while let Ok(message) = iopub_socket.read().await {
                        iopub.send(message).await?;
                    }
                    anyhow::Ok(())
                }
            });

            let (mut control_request_tx, mut control_request_rx) =
                futures::channel::mpsc::channel(100);
            let (mut shell_request_tx, mut shell_request_rx) = futures::channel::mpsc::channel(100);

            let routing_task = cx.background_executor().spawn({
                async move {
                    while let Some(message) = request_rx.next().await {
                        match message.content {
                            JupyterMessageContent::DebugRequest(_)
                            | JupyterMessageContent::InterruptRequest(_)
                            | JupyterMessageContent::ShutdownRequest(_) => {
                                control_request_tx.send(message).await?;
                            }
                            _ => {
                                shell_request_tx.send(message).await?;
                            }
                        }
                    }
                    anyhow::Ok(())
                }
            });

            let shell_task = cx.background_executor().spawn({
                async move {
                    while let Some(message) = shell_request_rx.next().await {
                        shell_socket.send(message).await.ok();
                        let reply = shell_socket.read().await?;
                        shell_reply_tx.send(reply).await?;
                    }
                    anyhow::Ok(())
                }
            });

            let control_task = cx.background_executor().spawn({
                async move {
                    while let Some(message) = control_request_rx.next().await {
                        control_socket.send(message).await.ok();
                        let reply = control_socket.read().await?;
                        control_reply_tx.send(reply).await?;
                    }
                    anyhow::Ok(())
                }
            });

            anyhow::Ok((
                Self {
                    process,
                    request_tx,
                    shell_task,
                    iopub_task,
                    control_task,
                    routing_task,
                    connection_path,
                    execution_state: ExecutionState::Busy,
                    kernel_info: None,
                },
                messages_rx,
            ))
        })
    }
}

impl Drop for RunningKernel {
    fn drop(&mut self) {
        std::fs::remove_file(&self.connection_path).ok();

        self.request_tx.close_channel();
    }
}

async fn read_kernelspec_at(
    // Path should be a directory to a jupyter kernelspec, as in
    // /usr/local/share/jupyter/kernels/python3
    kernel_dir: PathBuf,
    fs: &dyn Fs,
) -> anyhow::Result<RuntimeSpecification> {
    let path = kernel_dir;
    let kernel_name = if let Some(kernel_name) = path.file_name() {
        kernel_name.to_string_lossy().to_string()
    } else {
        anyhow::bail!("Invalid kernelspec directory: {path:?}");
    };

    if !fs.is_dir(path.as_path()).await {
        anyhow::bail!("Not a directory: {path:?}");
    }

    let expected_kernel_json = path.join("kernel.json");
    let spec = fs.load(expected_kernel_json.as_path()).await?;
    let spec = serde_json::from_str::<JupyterKernelspec>(&spec)?;

    Ok(RuntimeSpecification {
        name: kernel_name,
        path,
        kernelspec: spec,
    })
}

/// Read a directory of kernelspec directories
async fn read_kernels_dir(path: PathBuf, fs: &dyn Fs) -> anyhow::Result<Vec<RuntimeSpecification>> {
    let mut kernelspec_dirs = fs.read_dir(&path).await?;

    let mut valid_kernelspecs = Vec::new();
    while let Some(path) = kernelspec_dirs.next().await {
        match path {
            Ok(path) => {
                if fs.is_dir(path.as_path()).await {
                    if let Ok(kernelspec) = read_kernelspec_at(path, fs).await {
                        valid_kernelspecs.push(kernelspec);
                    }
                }
            }
            Err(err) => log::warn!("Error reading kernelspec directory: {err:?}"),
        }
    }

    Ok(valid_kernelspecs)
}

pub async fn get_runtime_specifications(
    fs: Arc<dyn Fs>,
) -> anyhow::Result<Vec<RuntimeSpecification>> {
    let data_dirs = dirs::data_dirs();
    let kernel_dirs = data_dirs
        .iter()
        .map(|dir| dir.join("kernels"))
        .map(|path| read_kernels_dir(path, fs.as_ref()))
        .collect::<Vec<_>>();

    let kernel_dirs = futures::future::join_all(kernel_dirs).await;
    let kernel_dirs = kernel_dirs
        .into_iter()
        .filter_map(Result::ok)
        .flatten()
        .collect::<Vec<_>>();

    Ok(kernel_dirs)
}

#[cfg(test)]
mod test {
    use super::*;
    use std::path::PathBuf;

    use gpui::TestAppContext;
    use project::FakeFs;
    use serde_json::json;

    #[gpui::test]
    async fn test_get_kernelspecs(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/jupyter",
            json!({
                ".zed": {
                    "settings.json": r#"{ "tab_size": 8 }"#,
                    "tasks.json": r#"[{
                        "label": "cargo check",
                        "command": "cargo",
                        "args": ["check", "--all"]
                    },]"#,
                },
                "kernels": {
                    "python": {
                        "kernel.json": r#"{
                            "display_name": "Python 3",
                            "language": "python",
                            "argv": ["python3", "-m", "ipykernel_launcher", "-f", "{connection_file}"],
                            "env": {}
                        }"#
                    },
                    "deno": {
                        "kernel.json": r#"{
                            "display_name": "Deno",
                            "language": "typescript",
                            "argv": ["deno", "run", "--unstable", "--allow-net", "--allow-read", "https://deno.land/std/http/file_server.ts", "{connection_file}"],
                            "env": {}
                        }"#
                    }
                },
            }),
        )
        .await;

        let mut kernels = read_kernels_dir(PathBuf::from("/jupyter/kernels"), fs.as_ref())
            .await
            .unwrap();

        kernels.sort_by(|a, b| a.name.cmp(&b.name));

        assert_eq!(
            kernels.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
            vec!["deno", "python"]
        );
    }
}
