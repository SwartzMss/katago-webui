use anyhow::{Context, Result, anyhow};
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::{Duration, timeout};

/// 简化的 GTP 引擎实例：提供最基本的命令往返
#[derive(Debug)]
pub struct GtpEngine {
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    stdout: Mutex<BufReader<ChildStdout>>, // 顺序读取响应
}

impl GtpEngine {
    /// 启动 kataGo gtp 进程
    pub async fn start(cmd_path: &str, args: &[String]) -> Result<Arc<Self>> {
        let mut cmd = Command::new(cmd_path);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        let mut child = cmd.spawn().context("failed to spawn katago gtp")?;
        if let Some(id) = child.id() {
            tracing::info!(pid=%id, "katago spawned");
        }
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("failed to open stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("failed to open stdout"))?;

        let engine = Arc::new(Self {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            stdout: Mutex::new(BufReader::new(stdout)),
        });

        // 简单握手：version（可选）
        // let _ = engine.send_command("version").await?;
        Ok(engine)
    }

    /// 发送单条 GTP 命令并读取响应（以 \n\n 结束）
    pub async fn send_command(self: &Arc<Self>, cmd: &str) -> Result<String> {
        let mut stdin = self.stdin.lock().await;
        let mut stdout = self.stdout.lock().await;

        let line = format!("{}\n", cmd);
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;

        // 读取直到空行
        let mut buf = String::new();
        let mut acc = String::new();
        loop {
            buf.clear();
            let n = stdout.read_line(&mut buf).await?;
            if n == 0 {
                break; // EOF
            }
            if buf.trim().is_empty() {
                break; // 响应结束
            }
            acc.push_str(&buf);
        }
        if acc.starts_with('?') {
            return Err(anyhow!("gtp error: {}", acc.trim()));
        }
        Ok(acc)
    }

    /// 优雅退出并等待子进程结束；超时则强杀
    pub async fn quit(self: &Arc<Self>) -> Result<()> {
        // 尝试优雅退出
        let _ = self.send_command("quit").await;

        // 等待最多 3 秒退出
        let mut child = self.child.lock().await;
        match timeout(Duration::from_secs(3), child.wait()).await {
            Ok(_status) => {
                if let Some(id) = child.id() {
                    tracing::info!(pid=%id, "katago exited");
                }
                return Ok(());
            }
            Err(_) => {
                // 超时，强制杀死
                let _ = child.kill().await;
                let _ = child.wait().await; // reap
                if let Some(id) = child.id() {
                    tracing::warn!(pid=%id, "katago killed after timeout");
                }
                return Ok(());
            }
        }
    }
}
