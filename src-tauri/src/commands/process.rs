//! Process detection commands

use std::process::Command;
use std::time::{Duration, Instant};

#[path = "desktop_reopen.rs"]
mod desktop_reopen;
pub use desktop_reopen::*;

#[cfg(any(windows, test))]
use anyhow::Context;

#[cfg(unix)]
use std::collections::HashMap;

#[cfg(any(unix, windows, test))]
use std::collections::HashSet;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

#[cfg(any(windows, test))]
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WindowsCodexProcess {
    name: String,
    process_id: u32,
    parent_process_id: u32,
    #[serde(default)]
    command_line: String,
    #[serde(default)]
    executable_path: String,
    #[serde(default)]
    main_window_title: String,
}

/// Information about running Codex processes
#[derive(Debug, Clone, serde::Serialize)]
pub struct CodexProcessInfo {
    /// Number of blocking desktop or independently owned Codex processes.
    pub count: usize,
    /// Backends owned by a verified desktop (legacy ignored helpers on other platforms).
    pub background_count: usize,
    /// Codex CLI, IDE or daemon processes that this app must never close.
    pub external_count: usize,
    /// Whether switching is allowed (no possibly shared Codex authentication users).
    pub can_switch: bool,
    /// Process IDs that must exit before switching the shared account.
    pub pids: Vec<u32>,
}

/// Only executable paths and process metadata leave the backend, never command arguments.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CodexProcessDetail {
    pub pid: u32,
    pub parent_pid: u32,
    pub name: String,
    pub kind: String,
    pub executable_path: String,
    pub can_stop: bool,
    pub stop_disabled_reason: Option<String>,
    pub identity: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct StopCodexProcessResult {
    pub pid: u32,
    pub stopped: bool,
    pub still_running: bool,
    pub message: String,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, PartialEq, Eq)]
struct MacKernelIdentity {
    pid: u32,
    parent_pid: u32,
    uid: u32,
    real_uid: u32,
    start_seconds: u64,
    start_microseconds: u64,
    executable_path: String,
}

// ABI from the macOS SDK's sys/proc_info.h (PROC_PIDTBSDINFO = 3).
#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Default)]
struct MacBsdInfo {
    flags: u32,
    status: u32,
    exit_status: u32,
    pid: u32,
    parent_pid: u32,
    uid: u32,
    gid: u32,
    real_uid: u32,
    real_gid: u32,
    saved_uid: u32,
    saved_gid: u32,
    reserved: u32,
    comm: [i8; 16],
    name: [i8; 32],
    files: u32,
    group: u32,
    job_count: u32,
    terminal: u32,
    terminal_group: u32,
    nice: i32,
    start_seconds: u64,
    start_microseconds: u64,
}

#[cfg(target_os = "macos")]
#[link(name = "proc")]
unsafe extern "C" {
    fn proc_pidinfo(
        pid: i32,
        flavor: i32,
        arg: u64,
        buffer: *mut std::ffi::c_void,
        size: i32,
    ) -> i32;
    fn proc_pidpath(pid: i32, buffer: *mut std::ffi::c_void, size: u32) -> i32;
}
#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn getuid() -> u32;
    #[link_name = "kill"]
    fn signal_single_process(pid: i32, signal: i32) -> i32;
}

#[cfg(target_os = "macos")]
fn read_macos_bsd_info(pid: u32) -> Result<Option<MacBsdInfo>, String> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Err("进程编号无效。".into());
    }
    let mut info = MacBsdInfo::default();
    let size = std::mem::size_of::<MacBsdInfo>();
    let received = unsafe {
        proc_pidinfo(
            pid as i32,
            3,
            0,
            &mut info as *mut _ as *mut std::ffi::c_void,
            size as i32,
        )
    };
    if received == size as i32 && info.pid == pid && info.start_seconds != 0 {
        return Ok(Some(info));
    }
    if received == 0 && std::io::Error::last_os_error().raw_os_error() == Some(3) {
        return Ok(None); // ESRCH: this process has exited.
    }
    Err("无法核验进程的启动时间和所属用户，请刷新后重试。".into())
}

#[cfg(target_os = "macos")]
fn read_macos_kernel_identity(pid: u32) -> Result<Option<MacKernelIdentity>, String> {
    let Some(info) = read_macos_bsd_info(pid)? else {
        return Ok(None);
    };
    let mut buffer = [0u8; 4096];
    let received = unsafe {
        proc_pidpath(
            pid as i32,
            buffer.as_mut_ptr() as *mut std::ffi::c_void,
            buffer.len() as u32,
        )
    };
    if received <= 0 {
        if read_macos_bsd_info(pid)?.is_none() {
            return Ok(None);
        }
        return Err("无法核验进程的可执行文件路径，禁止停止。".into());
    }
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    let path = std::str::from_utf8(&buffer[..end])
        .map_err(|_| "进程路径无法识别，禁止停止。".to_string())?;
    if !path.starts_with('/') {
        return Err("进程路径不完整，禁止停止。".into());
    }
    Ok(Some(MacKernelIdentity {
        pid,
        parent_pid: info.parent_pid,
        uid: info.uid,
        real_uid: info.real_uid,
        start_seconds: info.start_seconds,
        start_microseconds: info.start_microseconds,
        executable_path: path.to_string(),
    }))
}

#[cfg(target_os = "macos")]
fn protected_process_ancestors() -> Result<HashSet<u32>, String> {
    let mut protected = HashSet::new();
    let mut pid = std::process::id();
    while pid > 1 && protected.insert(pid) {
        let info = read_macos_bsd_info(pid)?
            .ok_or_else(|| "无法核验本工具的父进程，暂不允许手动停止。".to_string())?;
        pid = info.parent_pid;
    }
    protected.insert(0);
    protected.insert(1);
    Ok(protected)
}

#[cfg(target_os = "macos")]
#[derive(Clone)]
struct ManualStopTicket {
    process: MacKernelIdentity,
    issued_at: Instant,
}

#[cfg(target_os = "macos")]
static MANUAL_STOP_TICKETS: std::sync::LazyLock<
    std::sync::Mutex<HashMap<String, ManualStopTicket>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

#[cfg(target_os = "macos")]
fn manual_stop_disabled_reason(
    process: &MacProcessSnapshot,
    identity: &MacKernelIdentity,
    classification: &MacProcessClassification,
    protected: &HashSet<u32>,
    current_uid: u32,
) -> Option<String> {
    if protected.contains(&process.pid) {
        return Some("这是本工具或其父进程，不能在这里停止。".into());
    }
    if identity.uid != current_uid || identity.real_uid != current_uid {
        return Some("该进程不属于当前用户，不能在这里停止。".into());
    }
    if classification.desktop_pids.contains(&process.pid)
        || desktop_reopen::mac_bundle_path(&process.command, Some(&process.name)).is_some()
    {
        return Some("桌面客户端请通过“正常关闭 Codex”退出。".into());
    }
    if !classification.external_pids.contains(&process.pid) {
        return Some("这是桌面客户端所属服务，不能单独停止。".into());
    }
    let binary = std::path::Path::new(&identity.executable_path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let known_binary = matches!(binary, "codex" | "codex-app-server" | "codex-daemon");
    let npm_launcher = binary == "node"
        && process
            .command
            .split_whitespace()
            .any(|part| part.ends_with("/@openai/codex/bin/codex.js"));
    if !known_binary && !npm_launcher {
        return Some("无法确认这是 Codex 可执行文件，禁止停止。".into());
    }
    None
}

#[cfg(target_os = "macos")]
fn process_detail_kind(process: &MacProcessSnapshot, desktop: bool) -> String {
    if desktop {
        return "desktop".into();
    }
    let command = process.command.to_ascii_lowercase();
    if is_ide_plugin_process(&command) {
        "ide"
    } else if command.split_whitespace().any(|part| part == "daemon")
        || process.name == "codex-daemon"
    {
        "daemon"
    } else if command.split_whitespace().any(|part| part == "app-server")
        || process.name == "codex-app-server"
    {
        "app_server"
    } else {
        "cli"
    }
    .into()
}

#[tauri::command]
pub async fn list_codex_process_details() -> Result<Vec<CodexProcessDetail>, String> {
    #[cfg(target_os = "macos")]
    {
        return tokio::task::spawn_blocking(|| {
            let processes = read_macos_process_snapshot()
                .map_err(|_| "无法读取进程列表，请刷新后重试。".to_string())?;
            let classification = classify_macos_processes(&processes);
            let blocking: HashSet<u32> = classification.blocking_pids().into_iter().collect();
            let protected = protected_process_ancestors();
            let current_uid = unsafe { getuid() };
            let mut tickets = MANUAL_STOP_TICKETS
                .lock()
                .map_err(|_| "进程操作状态不可用。".to_string())?;
            tickets.retain(|_, ticket| ticket.issued_at.elapsed() < Duration::from_secs(60));
            if tickets.len() > 1024 {
                tickets.clear();
            }
            let mut details = Vec::new();
            for process in processes
                .iter()
                .filter(|process| blocking.contains(&process.pid))
            {
                let verified = read_macos_kernel_identity(process.pid);
                let (path, reason, identity_token) = match verified {
                    Ok(Some(identity)) => {
                        let reason = match &protected {
                            Ok(protected) => manual_stop_disabled_reason(
                                process,
                                &identity,
                                &classification,
                                protected,
                                current_uid,
                            ),
                            Err(reason) => Some(reason.clone()),
                        };
                        let token = if reason.is_none() {
                            let token = uuid::Uuid::new_v4().to_string();
                            tickets.insert(
                                token.clone(),
                                ManualStopTicket {
                                    process: identity.clone(),
                                    issued_at: Instant::now(),
                                },
                            );
                            Some(token)
                        } else {
                            None
                        };
                        (identity.executable_path, reason, token)
                    }
                    Ok(None) => continue,
                    Err(reason) => (String::new(), Some(reason), None),
                };
                details.push(CodexProcessDetail {
                    pid: process.pid,
                    parent_pid: process.parent_pid,
                    name: process.name.clone(),
                    kind: process_detail_kind(
                        process,
                        classification.desktop_pids.contains(&process.pid),
                    ),
                    executable_path: path,
                    can_stop: reason.is_none(),
                    stop_disabled_reason: reason,
                    identity: identity_token,
                });
            }
            details.sort_by_key(|detail| detail.pid);
            Ok(details)
        })
        .await
        .map_err(|_| "读取进程列表失败。".to_string())?;
    }
    #[cfg(not(target_os = "macos"))]
    Err("进程详情与单独停止功能目前仅支持 macOS。".into())
}

#[cfg(target_os = "macos")]
fn perform_manual_stop(
    requested_pid: u32,
    ticket: ManualStopTicket,
    current: Option<MacKernelIdentity>,
    disabled_reason: Option<String>,
    signal: impl FnOnce(u32) -> Result<(), String>,
    wait_for_exit: impl FnOnce(&MacKernelIdentity) -> Result<bool, String>,
) -> Result<StopCodexProcessResult, String> {
    if ticket.process.pid != requested_pid || ticket.issued_at.elapsed() >= Duration::from_secs(60)
    {
        return Err("进程确认信息已失效，请刷新列表后重新选择。".into());
    }
    let Some(current) = current else {
        return Ok(StopCodexProcessResult {
            pid: requested_pid,
            stopped: true,
            still_running: false,
            message: "该进程已自行退出，未发送停止信号。".into(),
        });
    };
    if current != ticket.process {
        return Err("进程身份已变化，可能已退出或 PID 被复用。未发送停止信号，请刷新列表。".into());
    }
    if let Some(reason) = disabled_reason {
        return Err(reason);
    }
    signal(requested_pid)?;
    let stopped = wait_for_exit(&current)?;
    Ok(StopCodexProcessResult { pid: requested_pid, stopped, still_running: !stopped,
        message: if stopped { "所选进程已停止。" } else { "已向所选进程发送正常停止请求，但它仍在运行。请在原程序中结束任务；本工具不会强制结束它。" }.into() })
}

#[tauri::command]
pub async fn stop_codex_process(
    pid: u32,
    identity: String,
) -> Result<StopCodexProcessResult, String> {
    #[cfg(target_os = "macos")]
    {
        return tokio::task::spawn_blocking(move || {
            let ticket = MANUAL_STOP_TICKETS.lock().map_err(|_| "进程操作状态不可用。".to_string())?
                .remove(&identity).ok_or_else(|| "进程确认信息已失效，请刷新列表后重新选择。".to_string())?;
            let processes = read_macos_process_snapshot().map_err(|_| "无法重新核验进程列表，未发送停止信号。".to_string())?;
            let classification = classify_macos_processes(&processes);
            let protected = protected_process_ancestors()?;
            let current = read_macos_kernel_identity(pid)?;
            let disabled = match (&current, processes.iter().find(|process| process.pid == pid)) {
                (Some(current), Some(process)) => manual_stop_disabled_reason(process, current, &classification, &protected, unsafe { getuid() }),
                (Some(_), None) => Some("进程不再属于已核验的 Codex 服务，未发送停止信号。".into()),
                (None, _) => None,
            };
            let expected_identity = ticket.process.clone();
            perform_manual_stop(pid, ticket, current, disabled, |pid| {
                // This narrowly scoped path is reached only after an explicit user action.
                // Recheck kernel identity immediately before SIGTERM; never signal a group.
                // Identity checks above include microsecond-resolution start time and UID.
                if read_macos_kernel_identity(pid)?.as_ref() != Some(&expected_identity) {
                    return Err("发送停止请求前进程身份已变化，未发送停止信号。请刷新列表。".into());
                }
                if unsafe { signal_single_process(pid as i32, 15) } != 0 {
                    return Err("正常停止请求未成功，可能进程已退出或系统不允许此操作。未尝试强制结束或提权。".into());
                }
                Ok(())
            }, |expected| {
                let started = Instant::now();
                loop {
                    match read_macos_bsd_info(expected.pid)? {
                        None => return Ok(true),
                        Some(current) if current.start_seconds != expected.start_seconds
                            || current.start_microseconds != expected.start_microseconds => return Ok(true),
                        Some(_) => {}
                    }
                    if started.elapsed() >= Duration::from_secs(5) { return Ok(false); }
                    std::thread::sleep(Duration::from_millis(100));
                }
            })
        }).await.map_err(|_| "停止进程操作未完成，请刷新进程列表确认状态。".to_string())?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (pid, identity);
        Err("单独停止服务目前仅支持 macOS。".into())
    }
}

/// Summary of a close operation for active Codex processes.
#[derive(Debug, Clone, serde::Serialize)]
pub struct KillCodexProcessesResult {
    /// Number of active Codex sessions targeted before expanding child processes.
    pub targeted_count: usize,
    /// Process IDs that were successfully force-signalled or observed closed.
    pub killed_pids: Vec<u32>,
    /// Process IDs that could not be terminated.
    pub failed_pids: Vec<u32>,
    pub reopen_token: Option<String>,
}

#[cfg(unix)]
struct UnixProcessSnapshot {
    children_by_parent: HashMap<u32, Vec<u32>>,
}

const CODEX_RUNNING_SWITCH_BLOCKED_PREFIX: &str = "Cannot switch accounts while ";

/// Check for running Codex processes
#[tauri::command]
pub async fn check_codex_processes() -> Result<CodexProcessInfo, String> {
    #[cfg(target_os = "macos")]
    let (pids, bg_count, external_count) = {
        let classification =
            classify_macos_processes(&read_macos_process_snapshot().map_err(|e| e.to_string())?);
        (
            classification.blocking_pids(),
            classification.owned_backend_count,
            classification.external_pids.len(),
        )
    };
    #[cfg(not(target_os = "macos"))]
    let (pids, bg_count, external_count) = {
        let (pids, count) = find_codex_processes().map_err(|e| e.to_string())?;
        (pids, count, 0)
    };
    let count = pids.len();

    Ok(CodexProcessInfo {
        count,
        background_count: bg_count,
        external_count,
        can_switch: count == 0,
        pids,
    })
}

pub(crate) fn ensure_codex_not_running() -> Result<(), String> {
    let (pids, _) = find_codex_processes().map_err(|e| e.to_string())?;

    if pids.is_empty() {
        return Ok(());
    }

    Err(format!(
        "{CODEX_RUNNING_SWITCH_BLOCKED_PREFIX}仍有 {} 个 Codex 进程使用当前登录，请先结束后再切换。",
        pids.len()
    ))
}

pub(crate) fn is_codex_running_switch_block(error: &str) -> bool {
    error.starts_with(CODEX_RUNNING_SWITCH_BLOCKED_PREFIX)
}

/// Close active Codex processes that currently block account switching.
/// Graceful close is the default; force close must be explicitly requested.
#[tauri::command]
pub async fn kill_codex_processes(
    reopen_desktop: Option<bool>,
    force_close: Option<bool>,
) -> Result<KillCodexProcessesResult, String> {
    tokio::task::spawn_blocking(move || {
        close_codex_processes_blocking(
            reopen_desktop.unwrap_or(false),
            force_close.unwrap_or(false),
        )
    })
    .await
    .map_err(|e| e.to_string())?
}

fn close_codex_processes_blocking(
    reopen_desktop: bool,
    force_close: bool,
) -> Result<KillCodexProcessesResult, String> {
    #[cfg(target_os = "macos")]
    {
        return close_macos_desktops(reopen_desktop, force_close);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let (pids, _) = find_codex_processes().map_err(|e| e.to_string())?;
        let desktops = if reopen_desktop {
            desktop_reopen::capture_desktops(&pids)?
        } else {
            Vec::new()
        };
        let targeted_count = pids.len();
        let mut killed_pids = Vec::new();
        let mut failed_pids = Vec::new();
        #[cfg(unix)]
        let snapshot = read_unix_process_snapshot();
        #[cfg(unix)]
        let targets = expand_process_targets(&pids, snapshot.as_ref());
        #[cfg(windows)]
        let targets = expand_process_targets(&pids);
        for pid in targets.iter().copied() {
            if close_process(pid, force_close) {
                killed_pids.push(pid);
            } else {
                failed_pids.push(pid);
            }
        }
        if !force_close {
            wait_for_processes_to_exit(&targets, Duration::from_secs(8));
            killed_pids = targets
                .iter()
                .copied()
                .filter(|pid| !process_exists(*pid))
                .collect();
            failed_pids = targets
                .iter()
                .copied()
                .filter(|pid| process_exists(*pid))
                .collect();
        }
        let reopen_token =
            desktop_reopen::remember_closed_desktops(desktops, &killed_pids, reopen_desktop);
        Ok(KillCodexProcessesResult {
            targeted_count,
            killed_pids,
            failed_pids,
            reopen_token,
        })
    }
}

#[cfg(target_os = "macos")]
fn close_macos_desktops(
    reopen_desktop: bool,
    force_close: bool,
) -> Result<KillCodexProcessesResult, String> {
    if force_close {
        return Err("macOS 已禁用强制关闭。请先完成任务，再正常退出 Codex。".into());
    }
    let processes = read_macos_process_snapshot().map_err(|e| e.to_string())?;
    let classification = classify_macos_processes(&processes);
    if !classification.external_pids.is_empty() {
        return Err(format!("仍有 Codex 命令行、编辑器服务或后台服务使用当前登录（进程编号：{}）。请先完成任务并自行关闭，再切换账号；本次未关闭任何进程。",
            classification.external_pids.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")));
    }
    let pids = &classification.desktop_pids;
    let desktops = desktop_reopen::capture_desktops(pids)?;
    let captured = desktop_reopen::desktop_pids(&desktops);
    if captured.len() != pids.len() || pids.iter().any(|pid| !captured.contains(pid)) {
        return Err(
            "无法核验所有 Codex 桌面客户端的身份。未发送关闭请求，请自行退出 Codex 后再切换。"
                .into(),
        );
    }
    let mut children_by_parent = HashMap::new();
    for process in &processes {
        children_by_parent
            .entry(process.parent_pid)
            .or_insert_with(Vec::new)
            .push(process.pid);
    }
    let snapshot = UnixProcessSnapshot { children_by_parent };
    // Descendants are observed only. No signal is sent to any child process.
    let targets = expand_process_targets(pids, Some(&snapshot));
    for desktop in &desktops {
        desktop_reopen::request_macos_desktop_quit(desktop)?;
    }
    wait_for_processes_to_exit(&targets, Duration::from_secs(8));
    let remaining = read_macos_process_snapshot().map_err(|e| e.to_string())?;
    let running: HashSet<u32> = remaining.iter().map(|process| process.pid).collect();
    let killed_pids = targets
        .iter()
        .copied()
        .filter(|pid| !running.contains(pid))
        .collect::<Vec<_>>();
    let failed_pids = targets
        .iter()
        .copied()
        .filter(|pid| running.contains(pid))
        .collect::<Vec<_>>();
    if !failed_pids.is_empty() {
        return Err(format!(
            "Codex 尚未完全退出（进程编号：{}）。未尝试强制关闭，请在原程序中结束这些进程后重试。",
            failed_pids
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let reopen_token =
        desktop_reopen::remember_closed_desktops(desktops, &killed_pids, reopen_desktop);
    Ok(KillCodexProcessesResult {
        targeted_count: pids.len(),
        killed_pids,
        failed_pids,
        reopen_token,
    })
}

#[cfg(unix)]
fn expand_process_targets(root_pids: &[u32], snapshot: Option<&UnixProcessSnapshot>) -> Vec<u32> {
    let mut targets = Vec::new();
    let mut visited = HashSet::new();

    if let Some(snapshot) = snapshot {
        for root_pid in root_pids {
            let mut stack = snapshot
                .children_by_parent
                .get(root_pid)
                .cloned()
                .unwrap_or_default();
            while let Some(pid) = stack.pop() {
                if !visited.insert(pid) {
                    continue;
                }
                targets.push(pid);

                if let Some(children) = snapshot.children_by_parent.get(&pid) {
                    stack.extend(children.iter().copied());
                }
            }
        }
    }

    for root_pid in root_pids {
        if visited.insert(*root_pid) {
            targets.push(*root_pid);
        }
    }

    targets
}

#[cfg(windows)]
fn expand_process_targets(root_pids: &[u32]) -> Vec<u32> {
    root_pids.to_vec()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn read_unix_process_snapshot() -> Option<UnixProcessSnapshot> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,ppid=,uid="])
        .output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut children_by_parent = HashMap::new();

    for line in stdout.lines() {
        let mut parts = line.split_whitespace();
        let Some(pid_str) = parts.next() else {
            continue;
        };
        let Some(ppid_str) = parts.next() else {
            continue;
        };
        let Some(uid_str) = parts.next() else {
            continue;
        };
        let (Ok(pid), Ok(ppid), Ok(_uid)) = (
            pid_str.parse::<u32>(),
            ppid_str.parse::<u32>(),
            uid_str.parse::<u32>(),
        ) else {
            continue;
        };

        children_by_parent
            .entry(ppid)
            .or_insert_with(Vec::new)
            .push(pid);
    }

    Some(UnixProcessSnapshot { children_by_parent })
}

#[cfg(not(target_os = "macos"))]
fn close_process(pid: u32, force: bool) -> bool {
    #[cfg(unix)]
    {
        let killed = Command::new("/bin/kill")
            .arg(if force { "-9" } else { "-TERM" })
            .arg(pid.to_string())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        return killed || !process_exists(pid);
    }

    #[cfg(windows)]
    {
        let mut command = Command::new("taskkill");
        command.creation_flags(CREATE_NO_WINDOW);
        if force {
            command.arg("/F");
        }
        let killed = command
            .args(["/T", "/PID", &pid.to_string()])
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        return killed || !process_exists(pid);
    }

    #[allow(unreachable_code)]
    false
}

fn wait_for_processes_to_exit(pids: &[u32], timeout: Duration) {
    let started = Instant::now();
    while pids.iter().any(|pid| process_exists(*pid)) && started.elapsed() < timeout {
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn process_exists(pid: u32) -> bool {
    #[cfg(unix)]
    {
        return Command::new("ps")
            .arg("-p")
            .arg(pid.to_string())
            .args(["-o", "pid="])
            .output()
            .map(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout)
                        .split_whitespace()
                        .any(|value| value == pid.to_string())
            })
            .unwrap_or(false);
    }

    #[cfg(windows)]
    {
        return Command::new("tasklist")
            .creation_flags(CREATE_NO_WINDOW)
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .map(|output| String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()))
            .unwrap_or(false);
    }

    #[allow(unreachable_code)]
    false
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone)]
struct MacProcessSnapshot {
    pid: u32,
    parent_pid: u32,
    name: String,
    command: String,
    bundle_identifier: Option<String>,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Default)]
struct MacProcessClassification {
    desktop_pids: Vec<u32>,
    external_pids: Vec<u32>,
    owned_backend_count: usize,
}

#[cfg(target_os = "macos")]
impl MacProcessClassification {
    fn blocking_pids(&self) -> Vec<u32> {
        let mut pids = self.desktop_pids.clone();
        pids.extend(&self.external_pids);
        pids.sort_unstable();
        pids.dedup();
        pids
    }
}

#[cfg(target_os = "macos")]
fn looks_like_codex_backend(process: &MacProcessSnapshot) -> bool {
    let name = process.name.to_ascii_lowercase();
    if matches!(
        name.as_str(),
        "codex" | "codex.exe" | "codex-app-server" | "codex-daemon"
    ) {
        return true;
    }
    let first = process.command.split_whitespace().next().unwrap_or("");
    if first == "codex" || first.ends_with("/codex") {
        return true;
    }
    // npm's launcher can run under node while starting the native CLI.
    if name == "node"
        && process
            .command
            .split_whitespace()
            .any(|part| part.ends_with("/codex.js"))
    {
        return true;
    }
    // A disappearing process-name snapshot must not hide a binary in a path with spaces.
    process.name.is_empty()
        && process.command.starts_with('/')
        && process
            .command
            .match_indices("/codex")
            .any(|(index, suffix)| {
                process.command[index + suffix.len()..]
                    .chars()
                    .next()
                    .is_none_or(char::is_whitespace)
            })
}

#[cfg(target_os = "macos")]
fn classify_macos_processes(processes: &[MacProcessSnapshot]) -> MacProcessClassification {
    let mut result = MacProcessClassification::default();
    let by_pid: HashMap<u32, &MacProcessSnapshot> = processes
        .iter()
        .map(|process| (process.pid, process))
        .collect();
    let desktops: HashMap<u32, String> = processes
        .iter()
        .filter_map(|process| {
            if process.bundle_identifier.as_deref() != Some("com.openai.codex") {
                return None;
            }
            desktop_reopen::mac_bundle_path(&process.command, Some(&process.name))
                .map(|bundle| (process.pid, bundle))
        })
        .collect();
    result.desktop_pids = desktops.keys().copied().collect();
    for process in processes {
        if desktops.contains_key(&process.pid) {
            continue;
        }
        let unverified_desktop =
            desktop_reopen::mac_bundle_path(&process.command, Some(&process.name)).is_some()
                && process.bundle_identifier.is_none();
        if !unverified_desktop && !looks_like_codex_backend(process) {
            continue;
        }
        let mut parent = process.parent_pid;
        let mut visited = HashSet::new();
        let mut owned = false;
        while visited.insert(parent) {
            if let Some(bundle) = desktops.get(&parent) {
                // Both ancestry and the bundled executable path are required. A CLI
                // started from a Codex terminal is still an independently owned session.
                owned = process.command.starts_with(&format!("{bundle}/Contents/"));
                break;
            }
            let Some(ancestor) = by_pid.get(&parent) else {
                break;
            };
            parent = ancestor.parent_pid;
        }
        if owned {
            result.owned_backend_count += 1;
        } else {
            result.external_pids.push(process.pid);
        }
    }
    result.desktop_pids.sort_unstable();
    result.external_pids.sort_unstable();
    result
}

#[cfg(target_os = "macos")]
fn read_macos_process_snapshot() -> anyhow::Result<Vec<MacProcessSnapshot>> {
    let names_output = Command::new("/bin/ps")
        .args(["-axo", "pid=,ucomm="])
        .output()?;
    anyhow::ensure!(
        names_output.status.success(),
        "无法读取进程名称，已暂停账号切换。"
    );
    let names: HashMap<u32, String> = String::from_utf8_lossy(&names_output.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.trim().splitn(2, char::is_whitespace);
            Some((
                parts.next()?.parse().ok()?,
                parts.next()?.trim().to_string(),
            ))
        })
        .collect();
    anyhow::ensure!(!names.is_empty(), "进程名称列表为空，已暂停账号切换。");
    let output = Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,command="])
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "无法检查运行中的进程，已暂停账号切换。"
    );
    let mut processes = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
    {
        let mut parts = line.trim().splitn(2, char::is_whitespace);
        let pid: u32 = parts.next().unwrap_or("").parse()?;
        let mut remainder = parts
            .next()
            .unwrap_or("")
            .trim_start()
            .splitn(2, char::is_whitespace);
        let parent_pid: u32 = remainder.next().unwrap_or("").parse()?;
        let command = remainder.next().unwrap_or("").trim_start().to_string();
        if pid == std::process::id() {
            continue;
        }
        let name = names.get(&pid).cloned().unwrap_or_else(|| {
            // A desktop started between the two snapshots still blocks switching.
            // Its identity will be checked from disk before it can be classified as owned.
            ["ChatGPT", "Codex"]
                .into_iter()
                .find(|name| desktop_reopen::mac_bundle_path(&command, Some(name)).is_some())
                .unwrap_or("")
                .to_string()
        });
        let bundle_identifier = read_macos_app_bundle_identifier(&command, Some(&name));
        processes.push(MacProcessSnapshot {
            pid,
            parent_pid,
            name,
            command,
            bundle_identifier,
        });
    }
    anyhow::ensure!(!processes.is_empty(), "进程列表为空，已暂停账号切换。");
    Ok(processes)
}

/// Find all running codex processes. Returns (active_pids, background_count)
fn find_codex_processes() -> anyhow::Result<(Vec<u32>, usize)> {
    #[cfg(target_os = "macos")]
    {
        let classification = classify_macos_processes(&read_macos_process_snapshot()?);
        return Ok((
            classification.blocking_pids(),
            classification.owned_backend_count,
        ));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut pids = Vec::new();
        let mut bg_count = 0;
        let process_names = read_unix_process_names();

        // Include TTY so we can distinguish interactive CLI sessions from
        // background helper processes such as lingering app-server instances.
        let output = Command::new("ps")
            .args(["-axo", "pid=,tty=,command="])
            .output();

        if let Ok(output) = output {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }

                let mut parts = line.split_whitespace();
                let Some(pid_str) = parts.next() else {
                    continue;
                };
                let Some(tty) = parts.next() else {
                    continue;
                };
                let command = parts.collect::<Vec<_>>().join(" ");
                if command.is_empty() {
                    continue;
                }

                let Ok(pid) = pid_str.parse::<u32>() else {
                    continue;
                };

                let lowercase_command = command.to_ascii_lowercase();
                let is_switcher = lowercase_command.contains("codex-switcher");

                if is_switcher {
                    continue;
                }

                // macOS app bundle paths can contain spaces (`Codex Helper.app`), so
                // splitting on whitespace can turn helper processes into false
                // positives for the main `Codex` app. Detect by full command shape
                // instead of relying on the first token.
                let first_token = command.split_whitespace().next().unwrap_or("");
                let is_codex_cli = first_token == "codex" || first_token.ends_with("/codex");
                let process_name = process_names.get(&pid).map(String::as_str);
                #[cfg(target_os = "macos")]
                let bundle_identifier = read_macos_app_bundle_identifier(&command, process_name);
                #[cfg(not(target_os = "macos"))]
                let bundle_identifier: Option<String> = None;
                let is_codex_desktop = is_macos_codex_desktop_process(
                    &command,
                    process_name,
                    bundle_identifier.as_deref(),
                );

                if !is_codex_cli && !is_codex_desktop {
                    continue;
                }

                if pid == std::process::id() || pids.contains(&pid) {
                    continue;
                }

                let is_ide_plugin = is_ide_plugin_process(&lowercase_command);
                let is_app_server = lowercase_command.contains("codex app-server");
                let has_tty = tty != "??" && tty != "?";

                if is_ide_plugin || is_app_server {
                    bg_count += 1;
                    continue;
                }

                if is_codex_desktop || has_tty {
                    pids.push(pid);
                } else {
                    // Headless or orphaned codex processes should not block switching.
                    bg_count += 1;
                }
            }
        }

        pids.sort_unstable();
        pids.dedup();

        return Ok((pids, bg_count));
    }

    #[cfg(windows)]
    {
        return find_windows_codex_processes();
    }

    #[allow(unreachable_code)]
    Ok((Vec::new(), 0))
}

#[cfg(unix)]
fn read_unix_process_names() -> HashMap<u32, String> {
    let Ok(output) = Command::new("ps").args(["-axo", "pid=,ucomm="]).output() else {
        return HashMap::new();
    };

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.trim().splitn(2, char::is_whitespace);
            let pid = parts.next()?.parse::<u32>().ok()?;
            let name = parts.next()?.trim();
            (!name.is_empty()).then(|| (pid, name.to_string()))
        })
        .collect()
}

#[cfg(unix)]
fn is_macos_codex_desktop_process(
    command: &str,
    process_name: Option<&str>,
    bundle_identifier: Option<&str>,
) -> bool {
    #[cfg(not(target_os = "macos"))]
    let _ = bundle_identifier;

    const LEGACY_EXECUTABLE_SUFFIX: &str = "/Codex.app/Contents/MacOS/Codex";
    #[cfg(target_os = "macos")]
    const CURRENT_EXECUTABLE_SUFFIX: &str = "/ChatGPT.app/Contents/MacOS/ChatGPT";
    #[cfg(target_os = "macos")]
    const CODEX_BUNDLE_IDENTIFIER: &str = "com.openai.codex";

    #[cfg(target_os = "macos")]
    if bundle_identifier != Some(CODEX_BUNDLE_IDENTIFIER) {
        return false;
    }
    let executable_suffix = match process_name {
        Some("Codex") => LEGACY_EXECUTABLE_SUFFIX,
        #[cfg(target_os = "macos")]
        Some("ChatGPT") if bundle_identifier == Some(CODEX_BUNDLE_IDENTIFIER) => {
            CURRENT_EXECUTABLE_SUFFIX
        }
        _ => return false,
    };

    command.find(executable_suffix).is_some_and(|index| {
        command[index + executable_suffix.len()..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace)
    })
}

#[cfg(target_os = "macos")]
fn read_macos_app_bundle_identifier(command: &str, process_name: Option<&str>) -> Option<String> {
    let bundle = desktop_reopen::mac_bundle_path(command, process_name)?;
    let info_plist = std::path::Path::new(&bundle).join("Contents/Info.plist");
    let value = plist::Value::from_file(info_plist).ok()?;

    value
        .as_dictionary()?
        .get("CFBundleIdentifier")?
        .as_string()
        .map(str::to_owned)
}

#[cfg(windows)]
fn find_windows_codex_processes() -> anyhow::Result<(Vec<u32>, usize)> {
    Ok(classify_windows_codex_processes(
        &read_windows_codex_processes()?,
    ))
}

#[cfg(windows)]
fn read_windows_codex_processes() -> anyhow::Result<Vec<WindowsCodexProcess>> {
    // tasklist counts every Electron helper (`--type=gpu-process`, crashpad, renderer, etc.),
    // which inflates the badge and incorrectly blocks switching. Use PowerShell so we can inspect
    // the command line and only count live top-level app instances.
    const POWERSHELL_SCRIPT: &str = r#"
$windowTitles = @{}
Get-Process -Name Codex,ChatGPT -ErrorAction SilentlyContinue | ForEach-Object {
  $windowTitles[[uint32]$_.Id] = $_.MainWindowTitle
}

Get-CimInstance Win32_Process |
  Where-Object { $_.Name -ieq 'Codex.exe' -or $_.Name -ieq 'ChatGPT.exe' } |
  ForEach-Object {
    [PSCustomObject]@{
      Name = $_.Name
      ProcessId = [uint32]$_.ProcessId
      ParentProcessId = [uint32]$_.ParentProcessId
      CommandLine = if ($_.CommandLine) { $_.CommandLine } else { '' }
      ExecutablePath = if ($_.ExecutablePath) { $_.ExecutablePath } else { '' }
      MainWindowTitle = if ($windowTitles.ContainsKey([uint32]$_.ProcessId)) {
        [string]$windowTitles[[uint32]$_.ProcessId]
      } else {
        ''
      }
    }
  } |
  ConvertTo-Json -Compress
"#;

    let output = Command::new("powershell.exe")
        .creation_flags(CREATE_NO_WINDOW)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            POWERSHELL_SCRIPT,
        ])
        .output()
        .context("failed to query Windows process list")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("PowerShell process query failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_windows_codex_processes(&stdout)
}

#[cfg(any(windows, test))]
fn classify_windows_codex_processes(processes: &[WindowsCodexProcess]) -> (Vec<u32>, usize) {
    let mut active_pids = Vec::new();
    let mut ignored_count = 0;

    for process in processes
        .iter()
        .filter(|process| is_windows_codex_root_process(process))
    {
        let command = process.command_line.to_ascii_lowercase();
        if is_ide_plugin_process(&command) {
            ignored_count += 1;
            continue;
        }

        let has_window = !process.main_window_title.trim().is_empty();
        let has_renderer =
            windows_has_descendant_matching(process.process_id, processes, |child| {
                child
                    .command_line
                    .to_ascii_lowercase()
                    .contains("--type=renderer")
            });
        let has_app_server =
            windows_has_descendant_matching(process.process_id, processes, |child| {
                let command = normalize_windows_path(&child.command_line);
                command.contains("resources\\codex.exe") && command.contains("app-server")
            });

        if has_window || has_renderer || has_app_server {
            active_pids.push(process.process_id);
        } else {
            // Ignore stale helper trees left behind after the window has already closed.
            ignored_count += 1;
        }
    }

    active_pids.sort_unstable();
    active_pids.dedup();

    (active_pids, ignored_count)
}

#[cfg(any(windows, test))]
fn parse_windows_codex_processes(stdout: &str) -> anyhow::Result<Vec<WindowsCodexProcess>> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    let value: serde_json::Value =
        serde_json::from_str(trimmed).context("failed to parse Windows process JSON")?;

    match value {
        serde_json::Value::Array(values) => values
            .into_iter()
            .map(|value| {
                serde_json::from_value(value)
                    .context("failed to deserialize Windows Codex process entry")
            })
            .collect(),
        value => Ok(vec![serde_json::from_value(value)
            .context("failed to deserialize Windows Codex process entry")?]),
    }
}

#[cfg(any(windows, test))]
fn is_windows_codex_root_process(process: &WindowsCodexProcess) -> bool {
    let name = process.name.to_ascii_lowercase();
    let command = normalize_windows_path(&process.command_line);

    if command.contains("codex-switcher") || command.contains("--type=") {
        return false;
    }

    if name == "codex.exe" {
        let executable_path = normalize_windows_path(&process.executable_path);
        return !command.contains("resources\\codex.exe")
            && !executable_path.contains("resources\\codex.exe");
    }

    name == "chatgpt.exe" && is_windows_codex_package_chatgpt_process(process)
}

#[cfg(any(windows, test))]
fn is_windows_codex_package_chatgpt_process(process: &WindowsCodexProcess) -> bool {
    let executable_path = process.executable_path.trim();
    if !executable_path.is_empty() {
        return is_windows_codex_package_chatgpt_path(executable_path);
    }

    windows_command_executable_path(&process.command_line)
        .is_some_and(is_windows_codex_package_chatgpt_path)
}

#[cfg(any(windows, test))]
fn is_windows_codex_package_chatgpt_path(path: &str) -> bool {
    let normalized = normalize_windows_path(path.trim().trim_matches('"'));
    let Some(package_path) = normalized.strip_suffix("\\app\\chatgpt.exe") else {
        return false;
    };
    let mut components = package_path.rsplit('\\');
    let Some(package_name) = components.next() else {
        return false;
    };
    let Some(package_parent) = components.next() else {
        return false;
    };

    package_parent == "windowsapps"
        && package_name.starts_with("openai.codex_")
        && package_name.ends_with("__2p2nqsd0c76g0")
}

#[cfg(any(windows, test))]
fn windows_command_executable_path(command: &str) -> Option<&str> {
    let command = command.trim_start();
    if let Some(quoted) = command.strip_prefix('"') {
        return quoted
            .split_once('"')
            .map(|(path, _)| path)
            .filter(|path| !path.is_empty());
    }

    command.split_whitespace().next()
}

#[cfg(any(windows, test))]
fn normalize_windows_path(value: &str) -> String {
    value.replace('/', "\\").to_ascii_lowercase()
}

#[cfg(any(unix, windows, test))]
fn is_ide_plugin_process(command: &str) -> bool {
    command.contains(".antigravity")
        || command.contains("openai.chatgpt")
        || command.contains(".vscode")
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::is_macos_codex_desktop_process;
    use super::{
        classify_windows_codex_processes, is_windows_codex_root_process,
        parse_windows_codex_processes, WindowsCodexProcess,
    };

    /// Explicitly opt in: reads live process metadata only, never signals or accesses auth.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "read-only live process diagnostics; run explicitly with --ignored --exact --nocapture"]
    async fn diagnose_live_process_details_read_only() {
        assert_eq!(std::mem::size_of::<super::MacBsdInfo>(), 136);
        let own_identity = super::read_macos_kernel_identity(std::process::id())
            .expect("read-only libproc self lookup must succeed")
            .expect("diagnostic process must still exist");
        let current_uid = unsafe { super::getuid() };
        assert_eq!(own_identity.uid, current_uid);
        assert_eq!(own_identity.real_uid, current_uid);
        assert!(own_identity.start_seconds > 0);
        assert!(std::path::Path::new(&own_identity.executable_path).is_absolute());

        let before = super::check_codex_processes()
            .await
            .expect("read-only count before details must succeed");
        let details = super::list_codex_process_details()
            .await
            .expect("read-only process details must succeed");
        let after = super::check_codex_processes()
            .await
            .expect("read-only count after details must succeed");
        println!(
            "read-only diagnostics: bsd_info_size=136 uid={current_uid} before_count={} details_count={} after_count={} external_count={} owned_backend_count={}",
            before.count, details.len(), after.count, after.external_count, after.background_count
        );
        let home = std::env::var("HOME").ok();
        for detail in &details {
            let path = home
                .as_deref()
                .and_then(|home| detail.executable_path.strip_prefix(home))
                .map(|suffix| format!("~{suffix}"))
                .unwrap_or_else(|| detail.executable_path.clone());
            println!(
                "pid={} kind={} can_stop={} reason={} path={}",
                detail.pid,
                detail.kind,
                detail.can_stop,
                detail.stop_disabled_reason.as_deref().unwrap_or("无"),
                path
            );
            if detail.can_stop {
                assert!(!detail.executable_path.is_empty());
                assert!(detail.stop_disabled_reason.is_none());
            }
        }
        if before.pids == after.pids {
            let detail_pids: Vec<u32> = details.iter().map(|detail| detail.pid).collect();
            assert_eq!(detail_pids, after.pids);
            println!("stable_snapshot_count_match=true");
        } else {
            println!("stable_snapshot_count_match=unavailable_due_to_process_churn");
        }
    }

    #[cfg(target_os = "macos")]
    fn synthetic_kernel_identity() -> super::MacKernelIdentity {
        super::MacKernelIdentity {
            pid: 4242,
            parent_pid: 42,
            uid: 501,
            real_uid: 501,
            start_seconds: 1800000000,
            start_microseconds: 123456,
            executable_path: "/fixture/codex".into(),
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn manual_stop_rejects_wrong_pid_reused_pid_expired_ticket_and_disabled_target() {
        let original = synthetic_kernel_identity();
        let signal_count = std::cell::Cell::new(0);
        for case in 0..4 {
            let mut current = original.clone();
            if case == 1 {
                current.start_microseconds += 1;
            }
            let ticket = super::ManualStopTicket {
                process: original.clone(),
                issued_at: if case == 2 {
                    std::time::Instant::now() - std::time::Duration::from_secs(61)
                } else {
                    std::time::Instant::now()
                },
            };
            let result = super::perform_manual_stop(
                if case == 0 { 4243 } else { 4242 },
                ticket,
                Some(current),
                if case == 3 {
                    Some("受保护进程".into())
                } else {
                    None
                },
                |_| {
                    signal_count.set(signal_count.get() + 1);
                    Ok(())
                },
                |_| panic!("must not wait for rejected target"),
            );
            assert!(result.is_err());
        }
        assert_eq!(signal_count.get(), 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn manual_stop_signals_only_the_selected_pid_and_reports_mock_exit_state() {
        for did_exit in [true, false] {
            let current = synthetic_kernel_identity();
            let ticket = super::ManualStopTicket {
                process: current.clone(),
                issued_at: std::time::Instant::now(),
            };
            let signal_count = std::cell::Cell::new(0);
            let result = super::perform_manual_stop(
                4242,
                ticket,
                Some(current),
                None,
                |pid| {
                    assert_eq!(pid, 4242);
                    signal_count.set(signal_count.get() + 1);
                    Ok(())
                },
                |_| Ok(did_exit),
            )
            .unwrap();
            assert_eq!(signal_count.get(), 1);
            assert_eq!(result.stopped, did_exit);
            assert_eq!(result.still_running, !did_exit);
        }
        let ticket = super::ManualStopTicket {
            process: synthetic_kernel_identity(),
            issued_at: std::time::Instant::now(),
        };
        let result = super::perform_manual_stop(
            4242,
            ticket,
            None,
            None,
            |_| panic!("exited process must not receive a signal"),
            |_| panic!("must not wait"),
        )
        .unwrap();
        assert!(result.stopped);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn manual_stop_protects_ancestors_other_users_desktops_and_unverified_binaries() {
        let process = mac_process(4242, 42, "codex", "/fixture/codex app-server", None);
        let classification = super::classify_macos_processes(&[process.clone()]);
        let current = synthetic_kernel_identity();
        assert!(super::manual_stop_disabled_reason(
            &process,
            &current,
            &classification,
            &std::collections::HashSet::new(),
            501
        )
        .is_none());
        assert!(super::manual_stop_disabled_reason(
            &process,
            &current,
            &classification,
            &std::collections::HashSet::from([4242]),
            501
        )
        .is_some());
        assert!(super::manual_stop_disabled_reason(
            &process,
            &current,
            &classification,
            &std::collections::HashSet::new(),
            502
        )
        .is_some());
        let mut wrong_binary = current.clone();
        wrong_binary.executable_path = "/bin/sleep".into();
        assert!(super::manual_stop_disabled_reason(
            &process,
            &wrong_binary,
            &classification,
            &std::collections::HashSet::new(),
            501
        )
        .is_some());
        let desktop = mac_process(
            4242,
            42,
            "ChatGPT",
            "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT",
            Some("com.openai.codex"),
        );
        let desktops = super::classify_macos_processes(&[desktop.clone()]);
        assert!(super::manual_stop_disabled_reason(
            &desktop,
            &current,
            &desktops,
            &std::collections::HashSet::new(),
            501
        )
        .is_some());
    }

    #[cfg(target_os = "macos")]
    fn mac_process(
        pid: u32,
        parent_pid: u32,
        name: &str,
        command: &str,
        bundle: Option<&str>,
    ) -> super::MacProcessSnapshot {
        super::MacProcessSnapshot {
            pid,
            parent_pid,
            name: name.into(),
            command: command.into(),
            bundle_identifier: bundle.map(str::to_owned),
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_rejects_force_close_before_inspecting_or_touching_any_process() {
        let error = super::close_macos_desktops(false, true).unwrap_err();
        assert!(error.contains("已禁用强制关闭"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_blocks_independent_cli_daemon_and_ide_even_without_a_tty() {
        let processes = vec![
            mac_process(
                1,
                0,
                "ChatGPT",
                "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT",
                Some("com.openai.codex"),
            ),
            mac_process(
                2,
                1,
                "codex",
                "/Applications/ChatGPT.app/Contents/Resources/codex app-server",
                None,
            ),
            mac_process(3, 0, "codex", "/usr/local/bin/codex", None),
            mac_process(4, 0, "codex", "/usr/local/bin/codex app-server", None),
            mac_process(5, 0, "codex", "/usr/local/bin/codex daemon", None),
            mac_process(
                6,
                0,
                "codex",
                "/Users/test/.vscode/extensions/openai.chatgpt/bin/codex app-server",
                None,
            ),
            mac_process(
                7,
                0,
                "codex",
                "/Users/test/.antigravity/extensions/openai.chatgpt/bin/codex app-server",
                None,
            ),
        ];
        let classified = super::classify_macos_processes(&processes);
        assert_eq!(classified.desktop_pids, vec![1]);
        assert_eq!(classified.external_pids, vec![3, 4, 5, 6, 7]);
        assert_eq!(classified.owned_backend_count, 1);
        assert_eq!(classified.blocking_pids(), vec![1, 3, 4, 5, 6, 7]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_requires_bundle_identity_and_ancestry_before_claiming_a_backend() {
        let processes = vec![
            mac_process(
                1,
                0,
                "ChatGPT",
                "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT",
                Some("com.openai.codex"),
            ),
            mac_process(2, 1, "zsh", "/bin/zsh", None),
            mac_process(3, 2, "codex", "/usr/local/bin/codex", None),
            mac_process(
                4,
                0,
                "codex",
                "/Applications/ChatGPT.app/Contents/Resources/codex app-server",
                None,
            ),
            mac_process(
                5,
                0,
                "Codex",
                "/Applications/Codex.app/Contents/MacOS/Codex",
                None,
            ),
            mac_process(
                6,
                0,
                "ChatGPT",
                "/Other/ChatGPT.app/Contents/MacOS/ChatGPT",
                None,
            ),
            mac_process(
                7,
                0,
                "ChatGPT",
                "/Normal/ChatGPT.app/Contents/MacOS/ChatGPT",
                Some("com.openai.chat"),
            ),
            mac_process(
                8,
                0,
                "node",
                "/usr/local/bin/node /usr/local/lib/@openai/codex/bin/codex.js",
                None,
            ),
        ];
        let classified = super::classify_macos_processes(&processes);
        assert_eq!(classified.desktop_pids, vec![1]);
        assert_eq!(classified.external_pids, vec![3, 4, 5, 6, 8]);
        assert_eq!(classified.owned_backend_count, 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_does_not_mistake_command_arguments_or_other_chatgpt_for_the_desktop() {
        let processes = vec![
            mac_process(
                1,
                0,
                "zsh",
                "/bin/zsh -c echo /Applications/ChatGPT.app/Contents/MacOS/ChatGPT",
                Some("com.openai.codex"),
            ),
            mac_process(
                2,
                0,
                "ChatGPT",
                "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT",
                Some("com.openai.chat"),
            ),
            mac_process(
                3,
                0,
                "Codex Switcher",
                "/Applications/Codex Switcher.app/Contents/MacOS/codex-switcher",
                None,
            ),
        ];
        assert!(super::classify_macos_processes(&processes)
            .blocking_pids()
            .is_empty());
    }

    fn windows_process(
        name: &str,
        process_id: u32,
        parent_process_id: u32,
        executable_path: &str,
        command_line: &str,
        main_window_title: &str,
    ) -> WindowsCodexProcess {
        WindowsCodexProcess {
            name: name.to_string(),
            process_id,
            parent_process_id,
            command_line: command_line.to_string(),
            executable_path: executable_path.to_string(),
            main_window_title: main_window_title.to_string(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn detects_only_the_legacy_macos_codex_desktop_root_process() {
        assert!(is_macos_codex_desktop_process(
            "/Applications/Codex.app/Contents/MacOS/Codex",
            Some("Codex"),
            Some("com.openai.codex")
        ));
        assert!(is_macos_codex_desktop_process(
            "/Users/test/Applications With Spaces/Codex.app/Contents/MacOS/Codex --flag",
            Some("Codex"),
            Some("com.openai.codex")
        ));
        assert!(!is_macos_codex_desktop_process(
            "/Applications/Codex.app/Contents/Frameworks/Codex Framework.framework/Helpers/Codex (Service).app/Contents/MacOS/Codex (Service) --type=gpu-process",
            Some("Codex (Service)"),
            None
        ));
        assert!(!is_macos_codex_desktop_process(
            "/Applications/Codex.app/Contents/Frameworks/Codex Framework.framework/Helpers/Codex (Renderer).app/Contents/MacOS/Codex (Renderer) --type=renderer",
            Some("Codex (Renderer)"),
            None
        ));
        assert!(!is_macos_codex_desktop_process(
            "/Applications/Codex.app/Contents/Resources/codex app-server",
            Some("codex"),
            None
        ));
        assert!(!is_macos_codex_desktop_process(
            "/Applications/Codex.app/Contents/Frameworks/Codex Framework.framework/Helpers/Codex (Renderer).app/Contents/MacOS/Codex (Renderer) --app-executable /Applications/Codex.app/Contents/MacOS/Codex --type=renderer",
            Some("Codex (Renderer)"),
            None
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn detects_only_the_current_macos_codex_desktop_root_process() {
        assert!(is_macos_codex_desktop_process(
            "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT",
            Some("ChatGPT"),
            Some("com.openai.codex")
        ));
        assert!(is_macos_codex_desktop_process(
            "/Users/test/Applications With Spaces/ChatGPT.app/Contents/MacOS/ChatGPT --flag",
            Some("ChatGPT"),
            Some("com.openai.codex")
        ));
        assert!(!is_macos_codex_desktop_process(
            "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT",
            Some("ChatGPT"),
            Some("com.openai.chat")
        ));
        assert!(!is_macos_codex_desktop_process(
            "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT",
            Some("ChatGPT"),
            None
        ));
        assert!(!is_macos_codex_desktop_process(
            "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT",
            Some("Codex"),
            Some("com.openai.codex")
        ));
        assert!(!is_macos_codex_desktop_process(
            "/Applications/ChatGPT.app/Contents/Frameworks/Codex Framework.framework/Helpers/Codex (Renderer).app/Contents/MacOS/Codex (Renderer) --app-executable /Applications/ChatGPT.app/Contents/MacOS/ChatGPT --type=renderer",
            Some("Codex (Renderer)"),
            Some("com.openai.codex")
        ));
    }

    #[test]
    fn parses_legacy_and_current_windows_process_snapshots() {
        let processes = parse_windows_codex_processes(
            r#"[
                {"Name":"Codex.exe","ProcessId":10,"ParentProcessId":1,"CommandLine":"Codex.exe","MainWindowTitle":"Codex"},
                {"Name":"ChatGPT.exe","ProcessId":20,"ParentProcessId":1,"CommandLine":"ChatGPT.exe","ExecutablePath":"C:\\Program Files\\WindowsApps\\OpenAI.Codex_26.707.3748.0_x64__2p2nqsd0c76g0\\app\\ChatGPT.exe","MainWindowTitle":"Codex"}
            ]"#,
        )
        .expect("legacy and current process snapshots should parse");

        assert_eq!(processes.len(), 2);
        assert_eq!(processes[0].name, "Codex.exe");
        assert!(processes[0].executable_path.is_empty());
        assert_eq!(processes[1].name, "ChatGPT.exe");
        assert!(processes[1].executable_path.ends_with(r"\app\ChatGPT.exe"));
    }

    #[test]
    fn parses_single_windows_process_snapshot() {
        let processes = parse_windows_codex_processes(
            r#"{"Name":"Codex.exe","ProcessId":10,"ParentProcessId":1,"CommandLine":"Codex.exe","MainWindowTitle":"Codex"}"#,
        )
        .expect("single process snapshot should parse");

        assert_eq!(processes.len(), 1);
        assert_eq!(processes[0].process_id, 10);
    }

    #[test]
    fn windows_root_detection_supports_legacy_and_current_packages() {
        let legacy_root = windows_process(
            "Codex.exe",
            10,
            1,
            r"C:\Users\test\AppData\Local\Programs\Codex\Codex.exe",
            r#""C:\Users\test\AppData\Local\Programs\Codex\Codex.exe""#,
            "Codex",
        );
        let current_root = windows_process(
            "ChatGPT.exe",
            20,
            1,
            r"C:\Program Files\WindowsApps\OpenAI.Codex_26.707.3748.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe",
            r#""C:\Program Files\WindowsApps\OpenAI.Codex_26.707.3748.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe""#,
            "Codex",
        );
        let current_root_from_command = windows_process(
            "ChatGPT.exe",
            21,
            1,
            "",
            r#""D:\WindowsApps\OpenAI.Codex_26.707.9999.0_arm64__2p2nqsd0c76g0\app\ChatGPT.exe" --flag"#,
            "Codex",
        );

        assert!(is_windows_codex_root_process(&legacy_root));
        assert!(is_windows_codex_root_process(&current_root));
        assert!(is_windows_codex_root_process(&current_root_from_command));
    }

    #[test]
    fn windows_root_detection_rejects_helpers_backends_and_unrelated_chatgpt() {
        let bundled_backend = windows_process(
            "Codex.exe",
            30,
            20,
            r"C:\Program Files\WindowsApps\OpenAI.Codex_26.707.3748.0_x64__2p2nqsd0c76g0\app\resources\codex.exe",
            "",
            "",
        );
        let packaged_renderer = windows_process(
            "ChatGPT.exe",
            31,
            20,
            r"C:\Program Files\WindowsApps\OpenAI.Codex_26.707.3748.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe",
            r#""C:\Program Files\WindowsApps\OpenAI.Codex_26.707.3748.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe" --type=renderer"#,
            "",
        );
        let unrelated_chatgpt = windows_process(
            "ChatGPT.exe",
            32,
            1,
            r"C:\Program Files\WindowsApps\OpenAI.ChatGPT_26.707.3748.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe",
            r#""C:\Program Files\WindowsApps\OpenAI.ChatGPT_26.707.3748.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe""#,
            "ChatGPT",
        );
        let wrong_publisher = windows_process(
            "ChatGPT.exe",
            33,
            1,
            r"C:\Program Files\WindowsApps\OpenAI.Codex_26.707.3748.0_x64__notcodex\app\ChatGPT.exe",
            "",
            "Codex",
        );
        let lookalike_outside_windows_apps = windows_process(
            "ChatGPT.exe",
            34,
            1,
            r"C:\Temp\OpenAI.Codex_26.707.3748.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe",
            "",
            "Codex",
        );
        let spoofed_argument = windows_process(
            "ChatGPT.exe",
            35,
            1,
            "",
            r#""C:\Program Files\ChatGPT\ChatGPT.exe" --inspect "C:\Program Files\WindowsApps\OpenAI.Codex_26.707.3748.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe""#,
            "ChatGPT",
        );

        assert!(!is_windows_codex_root_process(&bundled_backend));
        assert!(!is_windows_codex_root_process(&packaged_renderer));
        assert!(!is_windows_codex_root_process(&unrelated_chatgpt));
        assert!(!is_windows_codex_root_process(&wrong_publisher));
        assert!(!is_windows_codex_root_process(
            &lookalike_outside_windows_apps
        ));
        assert!(!is_windows_codex_root_process(&spoofed_argument));
    }

    #[test]
    fn classifies_legacy_and_current_windows_trees_by_root_pid() {
        let processes = vec![
            windows_process(
                "Codex.exe",
                100,
                1,
                r"C:\Users\test\AppData\Local\Programs\Codex\Codex.exe",
                r#""C:\Users\test\AppData\Local\Programs\Codex\Codex.exe""#,
                "",
            ),
            windows_process(
                "Codex.exe",
                101,
                100,
                r"C:\Users\test\AppData\Local\Programs\Codex\Codex.exe",
                r#""C:\Users\test\AppData\Local\Programs\Codex\Codex.exe" --type=renderer"#,
                "",
            ),
            windows_process(
                "ChatGPT.exe",
                200,
                1,
                r"C:\Program Files\WindowsApps\OpenAI.Codex_26.707.3748.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe",
                r#""C:\Program Files\WindowsApps\OpenAI.Codex_26.707.3748.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe""#,
                "",
            ),
            windows_process(
                "Codex.exe",
                201,
                200,
                r"C:\Program Files\WindowsApps\OpenAI.Codex_26.707.3748.0_x64__2p2nqsd0c76g0\app\resources\codex.exe",
                r#""C:\Program Files\WindowsApps\OpenAI.Codex_26.707.3748.0_x64__2p2nqsd0c76g0\app\resources\codex.exe" app-server"#,
                "",
            ),
        ];

        assert_eq!(
            classify_windows_codex_processes(&processes),
            (vec![100, 200], 0)
        );
    }

    #[test]
    fn ignores_stale_legacy_and_current_windows_roots() {
        let processes = vec![
            windows_process(
                "Codex.exe",
                100,
                1,
                r"C:\Users\test\AppData\Local\Programs\Codex\Codex.exe",
                r#""C:\Users\test\AppData\Local\Programs\Codex\Codex.exe""#,
                "",
            ),
            windows_process(
                "ChatGPT.exe",
                200,
                1,
                r"C:\Program Files\WindowsApps\OpenAI.Codex_26.707.3748.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe",
                r#""C:\Program Files\WindowsApps\OpenAI.Codex_26.707.3748.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe""#,
                "",
            ),
        ];

        assert_eq!(classify_windows_codex_processes(&processes), (vec![], 2));
    }

    #[test]
    fn windows_codex_shortcut_filter_excludes_switcher() {
        assert!(super::is_windows_codex_shortcut_name("Codex.lnk"));
        assert!(super::is_windows_codex_shortcut_name("OpenAI Codex.lnk"));
        assert!(!super::is_windows_codex_shortcut_name("Codex Switcher.lnk"));
        assert!(!super::is_windows_codex_shortcut_name("codex-switcher.lnk"));
        assert!(!super::is_windows_codex_shortcut_name("Codex.txt"));
    }
}

#[cfg(any(windows, test))]
fn windows_has_descendant_matching<F>(
    root_pid: u32,
    processes: &[WindowsCodexProcess],
    mut predicate: F,
) -> bool
where
    F: FnMut(&WindowsCodexProcess) -> bool,
{
    let mut queue = vec![root_pid];
    let mut visited = HashSet::new();

    while let Some(parent_pid) = queue.pop() {
        for process in processes
            .iter()
            .filter(|process| process.parent_process_id == parent_pid)
        {
            if !visited.insert(process.process_id) {
                continue;
            }

            if predicate(process) {
                return true;
            }

            queue.push(process.process_id);
        }
    }

    false
}

/// Open the Codex desktop app if it is installed.
#[tauri::command]
pub async fn open_codex_app() -> Result<(), String> {
    tokio::task::spawn_blocking(open_codex_app_blocking)
        .await
        .map_err(|e| e.to_string())?
}

fn open_codex_app_blocking() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        if command_succeeds(Command::new("open").args(["-b", "com.openai.codex"])) {
            return Ok(());
        }

        if command_succeeds(Command::new("open").args(["-a", "Codex"])) {
            return Ok(());
        }

        return Err("未安装 Codex 桌面客户端，或无法打开它".to_string());
    }

    #[cfg(windows)]
    {
        if open_windows_registered_app() {
            return Ok(());
        }

        if let Some(path) = find_windows_codex_app() {
            if spawn_windows_codex_exe(&path) {
                return Ok(());
            }
        }

        for shortcut in find_windows_codex_shortcuts() {
            if open_windows_shortcut(&shortcut) {
                return Ok(());
            }
        }

        return Err("未安装 Codex 桌面客户端，或无法打开它".to_string());
    }

    #[allow(unreachable_code)]
    Err("打开 Codex 桌面客户端的功能仅支持 macOS 和 Windows".to_string())
}

fn command_succeeds(command: &mut Command) -> bool {
    command
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn find_windows_codex_app() -> Option<std::path::PathBuf> {
    let mut candidates = Vec::new();

    for key in ["LOCALAPPDATA", "ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(base) = std::env::var_os(key) {
            let base = std::path::PathBuf::from(base);
            candidates.push(base.join("Programs").join("Codex").join("Codex.exe"));
            candidates.push(base.join("Programs").join("codex").join("Codex.exe"));
            candidates.push(base.join("Codex").join("Codex.exe"));
            candidates.push(base.join("OpenAI").join("Codex").join("Codex.exe"));
            candidates.push(
                base.join("OpenAI")
                    .join("Codex")
                    .join("bin")
                    .join("codex.exe"),
            );
            candidates.push(base.join("OpenAI Codex").join("Codex.exe"));
            candidates.push(base.join("Codex Desktop").join("Codex.exe"));
        }
    }

    candidates.extend(find_windows_codex_apps_in_programs());
    candidates.extend(find_windows_codex_apps_in_package_cache());

    candidates
        .into_iter()
        .find(|path| path.is_file() && looks_like_windows_desktop_app(path))
}

#[cfg(windows)]
fn looks_like_windows_desktop_app(path: &std::path::Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };

    if is_windows_openai_codex_bin(path) {
        return true;
    }

    parent.join("resources").join("app.asar").is_file()
        || parent.join("resources").join("app").is_dir()
        || parent.join("resources").is_dir()
}

#[cfg(windows)]
fn is_windows_openai_codex_bin(path: &std::path::Path) -> bool {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };

    if !file_name.eq_ignore_ascii_case("codex.exe") {
        return false;
    }

    let normalized = path
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase();
    normalized.contains("\\openai\\codex\\bin\\codex.exe")
}

#[cfg(windows)]
fn spawn_windows_codex_exe(path: &std::path::Path) -> bool {
    let mut command = Command::new(path);
    command.creation_flags(CREATE_NO_WINDOW);
    if let Some(parent) = path.parent() {
        command.current_dir(parent);
    }
    command.spawn().is_ok()
}

#[cfg(windows)]
fn open_windows_registered_app() -> bool {
    let script = r#"
$app = Get-StartApps |
  Where-Object {
    $name = [string]$_.Name
    $appId = [string]$_.AppID
    $text = ($name + ' ' + $appId).ToLowerInvariant()
    $isSwitcher = $text.Contains('codex switcher') -or $text.Contains('codex-switcher') -or $text.Contains('lampese')
    $isCodex = $name -eq 'Codex' -or $name -eq 'OpenAI Codex' -or $appId -like 'OpenAI.Codex*' -or ($text.Contains('openai') -and $text.Contains('codex'))
    $isCodex -and -not $isSwitcher
  } |
  Sort-Object @{ Expression = {
    if ($_.Name -eq 'Codex') { 0 }
    elseif ($_.Name -eq 'OpenAI Codex') { 1 }
    elseif ($_.AppID -like 'OpenAI.Codex*') { 2 }
    else { 3 }
  } }, Name |
  Select-Object -First 1
if ($null -eq $app) { exit 1 }
Start-Process ("shell:AppsFolder\" + $app.AppID)
"#;

    let mut command = Command::new("powershell.exe");
    command.creation_flags(CREATE_NO_WINDOW);
    command.args(["-NoProfile", "-NonInteractive", "-Command", script]);
    command_succeeds(&mut command)
}

#[cfg(windows)]
fn find_windows_codex_shortcuts() -> Vec<std::path::PathBuf> {
    let mut candidates = Vec::new();

    for key in ["APPDATA", "ProgramData"] {
        if let Some(base) = std::env::var_os(key) {
            let programs = std::path::PathBuf::from(base)
                .join("Microsoft")
                .join("Windows")
                .join("Start Menu")
                .join("Programs");
            candidates.push(programs.join("Codex.lnk"));
            candidates.push(programs.join("OpenAI").join("Codex.lnk"));
            collect_windows_codex_shortcuts(&programs, &mut candidates, 0);
        }
    }

    candidates
        .into_iter()
        .filter(|path| path.is_file())
        .collect()
}

#[cfg(windows)]
fn open_windows_shortcut(path: &std::path::Path) -> bool {
    let mut command = Command::new("cmd.exe");
    command.creation_flags(CREATE_NO_WINDOW);
    command.arg("/C").arg("start").arg("").arg(path);
    command_succeeds(&mut command)
}

#[cfg(windows)]
fn find_windows_codex_apps_in_programs() -> Vec<std::path::PathBuf> {
    let mut candidates = Vec::new();

    let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") else {
        return candidates;
    };

    let programs = std::path::PathBuf::from(local_app_data).join("Programs");
    collect_windows_codex_apps(&programs, &mut candidates, 0);
    candidates
}

#[cfg(windows)]
fn find_windows_codex_apps_in_package_cache() -> Vec<std::path::PathBuf> {
    let mut candidates = Vec::new();

    let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") else {
        return candidates;
    };

    let packages = std::path::PathBuf::from(local_app_data).join("Packages");
    let Ok(entries) = std::fs::read_dir(packages) else {
        return candidates;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(dir_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };

        if !dir_name.to_ascii_lowercase().starts_with("openai.codex_") {
            continue;
        }

        candidates.push(
            path.join("LocalCache")
                .join("Local")
                .join("OpenAI")
                .join("Codex")
                .join("bin")
                .join("codex.exe"),
        );
    }

    candidates
}

#[cfg(windows)]
fn collect_windows_codex_apps(
    dir: &std::path::Path,
    candidates: &mut Vec<std::path::PathBuf>,
    depth: usize,
) {
    if depth > 2 {
        return;
    }

    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_windows_codex_apps(&path, candidates, depth + 1);
            continue;
        }

        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };

        if file_name.eq_ignore_ascii_case("Codex.exe") {
            candidates.push(path);
        }
    }
}

#[cfg(windows)]
fn collect_windows_codex_shortcuts(
    dir: &std::path::Path,
    candidates: &mut Vec<std::path::PathBuf>,
    depth: usize,
) {
    if depth > 3 {
        return;
    }

    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_windows_codex_shortcuts(&path, candidates, depth + 1);
            continue;
        }

        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };

        if is_windows_codex_shortcut_name(file_name) {
            candidates.push(path);
        }
    }
}

#[cfg(any(windows, test))]
fn is_windows_codex_shortcut_name(file_name: &str) -> bool {
    if !file_name
        .rsplit_once('.')
        .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("lnk"))
    {
        return false;
    }

    let shortcut_name = file_name
        .rsplit_once('.')
        .map(|(name, _)| name)
        .unwrap_or(file_name)
        .to_ascii_lowercase();

    if shortcut_name.contains("codex switcher")
        || shortcut_name.contains("codex-switcher")
        || shortcut_name.contains("switcher")
    {
        return false;
    }

    shortcut_name == "codex"
        || shortcut_name.starts_with("codex ")
        || shortcut_name.contains("openai codex")
        || (shortcut_name.contains("openai") && shortcut_name.contains("codex"))
}
