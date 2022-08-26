use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::select;
use tokio::sync::Mutex;

/// Wrapper for a single process.
pub struct ProcessHandle {
    /// Child process handle running the command. None if not currently running
    process: Arc<Mutex<Option<Child>>>,
    /// The name of the process
    name: String,
}

impl ProcessHandle {
    pub const RESTART_INTERVAL_S: u64 = 2;

    /// If auto_restart is true, the ProcessHandle will periodically check if the process is alive
    /// and restart it if not.
    async fn start_process(
        binary: PathBuf,
        args: &Vec<String>,
        name: String,
        auto_restart: bool,
    ) -> anyhow::Result<Self> {
        let mut cmd = Command::new(binary.clone());
        cmd.args(args);

        let mut interval = tokio::time::interval(Duration::from_secs(Self::RESTART_INTERVAL_S));

        let process = Self::run_cmd(&mut cmd, &name, &binary)?;

        let process = Arc::new(Mutex::new(Some(process)));
        let task_process = process.clone();
        let task_name = name.clone();

        tokio::spawn(async move {
            if !auto_restart {
                return;
            }
            loop {
                select!(
                    _ = interval.tick() => {
                        if !Self::check_alive_inner(&task_process).await {
                            println!("Restarting process {} that is no longer alive", task_name);
                            let process = Self::run_cmd(&mut cmd, &task_name, &binary).expect("Failed to re-run command");
                            *task_process.lock().await = Some(process);
                        }
                    }
                )
            }
        });

        Ok(Self { process, name })
    }

    fn run_cmd(cmd: &mut Command, name: &String, binary: &Path) -> anyhow::Result<Child> {
        cmd.spawn().map_err(|e| {
            anyhow!(
                "Failed to start {}. Does it exist at `{}`? Err: {e}",
                &name,
                binary.display(),
            )
        })
    }

    pub async fn kill(&mut self) -> Result<()> {
        if let Some(mut process) = self.process.lock().await.take() {
            process.kill()?
        } else {
            println!("Tried to kill process {} that is not alive", self.name);
        }
        Ok(())
    }

    pub async fn check_alive(&mut self) -> bool {
        Self::check_alive_inner(&self.process).await
    }

    async fn check_alive_inner(process: &Arc<Mutex<Option<Child>>>) -> bool {
        if let Some(ref mut process) = *process.lock().await {
            process.try_wait().unwrap().is_none()
        } else {
            false
        }
    }
}

/// Manages running a readyset-server binary with the correct arguments.
pub struct ReadysetServerBuilder {
    /// Path to the readyset-server binary.
    binary: PathBuf,

    /// The arguments to pass to the readyset-server process on startup.
    args: Vec<String>,

    /// Whether or not to automatically restart the server if it panics
    auto_restart: bool,
}

impl ReadysetServerBuilder {
    pub fn new(binary: &Path) -> Self {
        Self {
            binary: binary.to_owned(),
            args: vec!["--noria-metrics".into()],
            auto_restart: false,
        }
    }

    pub async fn start(&self) -> anyhow::Result<ProcessHandle> {
        ProcessHandle::start_process(
            self.binary.clone(),
            &self.args,
            "readyset-server".to_string(),
            self.auto_restart,
        )
        .await
    }

    fn push_arg(mut self, arg_name: &str) -> Self {
        self.args.push(arg_name.to_string());
        self
    }

    fn push_arg_kv(mut self, arg_name: &str, arg_value: &str) -> Self {
        self.args.push(arg_name.to_string());
        self.args.push(arg_value.to_string());
        self
    }

    pub fn volume_id(self, id: &str) -> Self {
        self.push_arg_kv("--volume-id", id)
    }

    pub fn reader_only(self) -> Self {
        self.push_arg("--reader-only")
    }

    pub fn no_readers(self) -> Self {
        self.push_arg("--no-readers")
    }

    pub fn authority_addr(self, authority_addr: &str) -> Self {
        self.push_arg_kv("--authority-address", authority_addr)
    }

    pub fn authority(self, authority: &str) -> Self {
        self.push_arg_kv("--authority", authority)
    }

    pub fn deployment(self, deployment: &str) -> Self {
        self.push_arg_kv("--deployment", deployment)
    }

    pub fn shards(self, shards: usize) -> Self {
        self.push_arg_kv("--shards", &shards.to_string())
    }

    pub fn quorum(self, quorum: usize) -> Self {
        self.push_arg_kv("--quorum", &quorum.to_string())
    }

    pub fn external_port(self, external_port: u16) -> Self {
        self.push_arg_kv("--external-port", &external_port.to_string())
    }

    pub fn mysql(self, addr: &str) -> Self {
        self.push_arg_kv("--replication-url", addr)
    }

    pub fn replicator_restart_timeout(self, timeout: u64) -> Self {
        self.push_arg_kv("--replicator-restart-timeout", &timeout.to_string())
    }

    pub fn reader_replicas(self, num_replicas: usize) -> Self {
        self.push_arg_kv("--reader-replicas", &num_replicas.to_string())
    }

    pub fn auto_restart(mut self, auto_restart: bool) -> Self {
        self.auto_restart = auto_restart;
        self
    }
}

/// Manages running a readyset-mysql binary with the correct arguments.
pub struct AdapterBuilder {
    /// Path to the readyset-mysql binary.
    binary: PathBuf,

    /// The arguments to pass to the readyset-mysql process on startup.
    args: Vec<String>,

    /// Whether or not to automatically restart the adapter if it panics
    auto_restart: bool,
}

impl AdapterBuilder {
    pub fn new(binary: &Path) -> Self {
        Self {
            binary: binary.to_owned(),
            args: vec![
                "--allow-unauthenticated-connections".to_string(),
                "--migration-request-timeout-ms".to_string(),
                "1000".to_string(),
            ],
            auto_restart: false,
        }
    }

    pub async fn start(&self) -> anyhow::Result<ProcessHandle> {
        ProcessHandle::start_process(
            self.binary.clone(),
            &self.args,
            "readyset-adapter".to_string(),
            self.auto_restart,
        )
        .await
    }

    fn push_arg(mut self, arg_name: &str, arg_value: &str) -> Self {
        self.args.push(arg_name.to_string());
        self.args.push(arg_value.to_string());
        self
    }

    pub fn authority_addr(self, authority_addr: &str) -> Self {
        self.push_arg("--authority-address", authority_addr)
    }

    pub fn authority(self, authority: &str) -> Self {
        self.push_arg("--authority", authority)
    }

    pub fn deployment(self, deployment: &str) -> Self {
        self.push_arg("--deployment", deployment)
    }

    pub fn port(self, port: u16) -> Self {
        self.push_arg("-a", &format!("127.0.0.1:{}", port))
    }

    pub fn metrics_port(self, port: u16) -> Self {
        self.push_arg("--metrics-address", &format!("0.0.0.0:{}", port))
    }

    pub fn mysql(self, addr: &str) -> Self {
        self.push_arg("--upstream-db-url", addr)
    }

    pub fn async_migrations(mut self, migration_task_interval: u64) -> Self {
        self.args.push("--async-migrations".to_string());
        self.push_arg(
            "--migration-task-interval",
            &migration_task_interval.to_string(),
        )
    }

    pub fn explicit_migrations(mut self, dry_run_task_interval: u64) -> Self {
        self.args.push("--explicit-migrations".to_string());
        self.push_arg(
            "--migration-task-interval",
            &dry_run_task_interval.to_string(),
        )
    }

    pub fn query_max_failure_seconds(self, secs: u64) -> Self {
        self.push_arg("--query-max-failure-seconds", &secs.to_string())
    }

    pub fn fallback_recovery_seconds(self, secs: u64) -> Self {
        self.push_arg("--fallback-recovery-seconds", &secs.to_string())
    }

    pub fn auto_restart(mut self, auto_restart: bool) -> Self {
        self.auto_restart = auto_restart;
        self
    }
}