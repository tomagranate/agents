use std::{
    env,
    ffi::OsString,
    fs,
    io::{self, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    thread::{self, JoinHandle},
};

use anyhow::{Context, Result, bail};
use chrono::Local;
use clap::Args;
use signal_hook::{
    consts::signal::{SIGHUP, SIGINT, SIGTERM},
    iterator::{Handle as SignalHandle, Signals},
    low_level,
};
use tempfile::NamedTempFile;

const RULE_PATH: &str = "/etc/sudoers.d/agents-session";
const SUDOERS_PATH: &str = "/etc/sudoers";
const TIMEOUT_MINUTES: u16 = 720;
const SUDO: &str = "/usr/bin/sudo";
const INSTALL: &str = "/usr/bin/install";
const RM: &str = "/usr/bin/rm";
const ID: &str = "/usr/bin/id";
const UNAME: &str = "/usr/bin/uname";
const VISUDO: &str = "/usr/sbin/visudo";

const BLOCK_START: &str = "# >>> agents op-ticket (managed by `agents sudo`) >>>";
const BLOCK_END: &str = "# <<< agents op-ticket <<<";
const REMOTE_OUTPUT_MARKER: &[u8] = b"__AGENTS_OUTPUT_START__\n";
const ZSHENV_BLOCK: &str = r#"# >>> agents op-ticket (managed by `agents sudo`) >>>
_op_ticket="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/op-ticket"
if [ -f "$_op_ticket" ]; then
  export OP_SERVICE_ACCOUNT_TOKEN="$(cat "$_op_ticket")"
fi
unset _op_ticket
# <<< agents op-ticket <<<
"#;

const REMOTE_IDENTITY: &str =
    r#"printf '__AGENTS_OUTPUT_START__\n%s\n%s\n' "$(uname -s)" "$(id -u)""#;
const REMOTE_WRITE_TICKET: &str = r#"umask 077; d="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"; f="$d/op-ticket"; t=$(mktemp "$d/op-ticket.XXXXXX") || exit 1; trap 'rm -f "$t"' EXIT HUP INT TERM; chmod 600 "$t" && cat > "$t" && mv -f "$t" "$f""#;
const REMOTE_REMOVE_TICKET: &str =
    r#"d="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"; rm -f "$d/op-ticket""#;
const REMOTE_TICKET_EXISTS: &str =
    r#"d="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"; test -f "$d/op-ticket""#;
const REMOTE_OP_ACTIVE: &str = r#"d="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"; f="$d/op-ticket"; test -f "$f" && OP_SERVICE_ACCOUNT_TOKEN="$(cat "$f")" op whoami >/dev/null 2>&1"#;
const REMOTE_READ_ZSHENV: &str = r#"printf '__AGENTS_OUTPUT_START__\n'; if [ -f "$HOME/.zshenv" ]; then cat "$HOME/.zshenv"; fi"#;
const REMOTE_WRITE_ZSHENV: &str = r#"target="$HOME/.zshenv"; temp=$(mktemp "$HOME/.zshenv.agents.XXXXXX") || exit 1; trap 'rm -f "$temp"' EXIT HUP INT TERM; if [ -e "$target" ]; then mode=$(stat -c '%a' "$target") || exit 1; else mode=600; fi; cat > "$temp" && chmod "$mode" "$temp" && mv -f "$temp" "$target""#;
const REMOTE_ZSHENV_EXISTS: &str = r#"test -f "$HOME/.zshenv""#;

#[derive(Args)]
pub struct SudoArgs {
    /// SSH machine name or alias. The default is this machine. user@host is not supported.
    pub machine: Option<String>,

    /// Report the sudo and 1Password ticket status.
    #[arg(long, conflicts_with_all = ["revoke", "remove"])]
    pub status: bool,

    /// Revoke both tickets.
    #[arg(long, conflicts_with_all = ["status", "remove"])]
    pub revoke: bool,

    /// Revoke tickets and remove managed configuration.
    #[arg(long, conflicts_with_all = ["status", "revoke"])]
    pub remove: bool,

    /// Run only the local sudo operation for a remote caller.
    #[arg(long, hide = true)]
    pub sudo_only: bool,
}

#[derive(Clone, Copy)]
enum Action {
    Grant,
    Status,
    Revoke,
    Remove,
}

#[derive(Clone, Copy)]
enum RemoteSudoAction {
    Grant,
    Revoke,
    Remove,
}

impl SudoArgs {
    fn action(&self) -> Action {
        if self.status {
            Action::Status
        } else if self.revoke {
            Action::Revoke
        } else if self.remove {
            Action::Remove
        } else {
            Action::Grant
        }
    }
}

pub fn run(args: SudoArgs) -> Result<()> {
    let action = args.action();
    if args.sudo_only {
        if args.machine.is_some() {
            bail!("--sudo-only cannot target another machine");
        }
        require_normal_user()?;
        require_linux(&command_text(UNAME, &["-s"])?)?;
        return run_local_sudo(action);
    }

    require_normal_user()?;
    if let Some(machine) = args.machine.as_deref() {
        validate_machine_name(machine)?;
        run_remote(machine, action)
    } else {
        require_linux(&command_text(UNAME, &["-s"])?)?;
        let machine = hostname::get()
            .context("could not read this machine name")?
            .into_string()
            .map_err(|_| anyhow::anyhow!("this machine name is not valid UTF-8"))?;
        validate_machine_name(&machine)?;
        run_local(&machine, action)
    }
}

fn run_local(machine: &str, action: Action) -> Result<()> {
    match action {
        Action::Grant => {
            println!("agents sudo → {machine} (this machine)");
            ensure_op_signed_in(true)?;
            install_sudo_ticket()?;
            if let Err(error) = install_local_op_ticket(machine) {
                eprintln!("⚠ sudo ticket active, but 1Password setup is incomplete");
                return Err(error);
            }
            println!("✓ 1Password ticket active (12 hours)");
            println!("\nRevoke with `agents sudo --revoke`");
            Ok(())
        }
        Action::Status => {
            let sudo_active = sudo_ticket_active();
            let ticket = local_ticket_path()?;
            let ticket_exists = ticket.is_file();
            let op_active = ticket_exists && local_op_ticket_active(&ticket);
            print_status(sudo_active, ticket_exists, op_active);
            if sudo_active && op_active {
                Ok(())
            } else {
                bail!("one or more tickets are inactive")
            }
        }
        Action::Revoke => {
            revoke_local_sudo()?;
            remove_file_if_present(&local_ticket_path()?)?;
            print_revoke_note();
            Ok(())
        }
        Action::Remove => {
            remove_local_sudo()?;
            remove_file_if_present(&local_ticket_path()?)?;
            remove_zshenv(&home_zshenv()?)?;
            print_revoke_note();
            Ok(())
        }
    }
}

fn run_remote(machine: &str, action: Action) -> Result<()> {
    require_remote_target(machine)?;
    match action {
        Action::Grant => {
            println!("agents sudo → {machine}");
            ensure_op_signed_in(false)?;
            run_remote_sudo(machine, RemoteSudoAction::Grant)?;
            if let Err(error) = install_remote_op_ticket(machine) {
                eprintln!("⚠ sudo ticket active on {machine}, but 1Password setup is incomplete");
                return Err(error);
            }
            println!("✓ 1Password ticket active on {machine} (12 hours)");
            println!("\nRevoke with `agents sudo --revoke {machine}`");
            Ok(())
        }
        Action::Status => {
            let sudo_active = remote_sudo_status(machine)?;
            let ticket_exists = ssh_status(machine, REMOTE_TICKET_EXISTS)?;
            let op_active = ticket_exists && ssh_status(machine, REMOTE_OP_ACTIVE)?;
            print_status(sudo_active, ticket_exists, op_active);
            if sudo_active && op_active {
                Ok(())
            } else {
                bail!("one or more tickets are inactive on {machine}")
            }
        }
        Action::Revoke => {
            run_remote_sudo(machine, RemoteSudoAction::Revoke)?;
            ssh_checked(machine, REMOTE_REMOVE_TICKET)
                .context("could not remove the remote 1Password ticket")?;
            print_revoke_note();
            Ok(())
        }
        Action::Remove => {
            run_remote_sudo(machine, RemoteSudoAction::Remove)?;
            ssh_checked(machine, REMOTE_REMOVE_TICKET)
                .context("could not remove the remote 1Password ticket")?;
            remove_remote_zshenv(machine)?;
            print_revoke_note();
            Ok(())
        }
    }
}

fn run_local_sudo(action: Action) -> Result<()> {
    match action {
        Action::Grant => install_sudo_ticket(),
        Action::Status => {
            let active = sudo_ticket_active();
            println!(
                "Sudo ticket: {}",
                if active { "active" } else { "inactive" }
            );
            if active {
                Ok(())
            } else {
                bail!("no active sudo ticket; run `agents sudo` in your terminal")
            }
        }
        Action::Revoke => revoke_local_sudo(),
        Action::Remove => remove_local_sudo(),
    }
}

fn install_sudo_ticket() -> Result<()> {
    let user = command_text(ID, &["-un"])?;
    validate_user_name(&user)?;

    let mut rule = NamedTempFile::new().context("could not create a temporary sudo policy")?;
    let _signal_cleanup = TempFileSignalCleanup::new(rule.path())?;
    writeln!(rule, "{}", sudoers_rule(&user))?;
    rule.flush()?;

    checked_status_quiet_stdout(VISUDO, &["-cf", &rule.path().display().to_string()])
        .context("generated sudo policy is invalid")?;

    println!("→ sudo authentication (installs {RULE_PATH})");
    checked_status(SUDO, &["-v"])?;
    checked_status(
        SUDO,
        &[
            INSTALL,
            "-o",
            "root",
            "-g",
            "root",
            "-m",
            "0440",
            &rule.path().display().to_string(),
            RULE_PATH,
        ],
    )?;
    checked_status_quiet_stdout(SUDO, &[VISUDO, "-cf", SUDOERS_PATH])
        .context("installed sudo policy did not validate")?;

    // Refresh after the policy changes the ticket scope from terminal to global.
    checked_status(SUDO, &["-v"])?;
    if !sudo_ticket_active() {
        bail!("global sudo ticket is not active");
    }

    println!("✓ sudo ticket active (12 hours)");
    Ok(())
}

fn sudoers_rule(user: &str) -> String {
    format!("Defaults:{user} timestamp_type=global, timestamp_timeout={TIMEOUT_MINUTES}")
}

fn sudo_ticket_active() -> bool {
    quiet_status(SUDO, &["-n", "true"]).unwrap_or(false)
}

fn revoke_local_sudo() -> Result<()> {
    checked_status(SUDO, &["-K"])?;
    println!("✓ sudo ticket revoked");
    Ok(())
}

fn remove_local_sudo() -> Result<()> {
    checked_status(SUDO, &[RM, "-f", RULE_PATH])?;
    checked_status(SUDO, &["-K"])?;
    println!("✓ removed {RULE_PATH} and revoked the sudo ticket");
    Ok(())
}

// Non-interactive ssh shells often miss `~/.local/bin` (where primer installs
// the CLI), so the remote command prepends it to PATH itself.
const REMOTE_AGENTS_SUDO: &str = "PATH=\"$HOME/.local/bin:$PATH\" agents sudo --sudo-only";

fn run_remote_sudo(machine: &str, action: RemoteSudoAction) -> Result<()> {
    let mut command = Command::new("ssh");
    // LogLevel=ERROR silences the "Connection to <machine> closed." noise from -t.
    command.args(["-o", "LogLevel=ERROR"]);
    if matches!(action, RemoteSudoAction::Grant | RemoteSudoAction::Remove) {
        command.arg("-t");
    }
    let mut remote = String::from(REMOTE_AGENTS_SUDO);
    match action {
        RemoteSudoAction::Grant => {}
        RemoteSudoAction::Revoke => remote.push_str(" --revoke"),
        RemoteSudoAction::Remove => remote.push_str(" --remove"),
    }
    command.arg(machine).arg(remote);
    let status = command
        .status()
        .with_context(|| format!("could not connect to {machine} with ssh"))?;
    check_remote_agents_status(machine, status)?;
    if !status.success() {
        bail!("remote sudo operation failed on {machine}");
    }
    Ok(())
}

fn remote_sudo_status(machine: &str) -> Result<bool> {
    let status = Command::new("ssh")
        .arg(machine)
        .arg(format!("{REMOTE_AGENTS_SUDO} --status"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("could not connect to {machine} with ssh"))?;
    check_remote_agents_status(machine, status)?;
    Ok(status.success())
}

fn check_remote_agents_status(machine: &str, status: ExitStatus) -> Result<()> {
    match status.code() {
        Some(255) => {
            bail!("could not connect to {machine}; run `ssh {machine}` to check access")
        }
        Some(126 | 127) => bail!(
            "agents is not installed or cannot run on {machine}; run `primer update` on {machine}"
        ),
        Some(2) => bail!(
            "agents on {machine} does not support `agents sudo`; run `primer update` on {machine}"
        ),
        _ => Ok(()),
    }
}

// The hint matters for local grants: with desktop-app integration, op prompts
// through the app GUI, which is invisible over ssh. Remote grants run op on the
// attended machine, so the prompt is in front of the user and the hint is noise.
fn ensure_op_signed_in(hint: bool) -> Result<()> {
    if hint {
        println!(
            "→ 1Password may prompt in the desktop app on this machine; \
             if you are connected remotely, run `agents sudo <machine>` from the machine you are at"
        );
    }
    if quiet_op_status(&["whoami"])? {
        return Ok(());
    }

    let status = op_command()
        .arg("signin")
        .status()
        .context("could not run `op signin`")?;
    if !status.success() || !quiet_op_status(&["whoami"])? {
        bail!("op is not signed in on this machine; run `op signin`");
    }
    Ok(())
}

fn mint_op_ticket(machine: &str) -> Result<Vec<u8>> {
    let name = format!(
        "op-ticket-{machine}-{}",
        Local::now().format("%Y%m%d-%H%M%S")
    );
    let output = op_command()
        .args([
            "service-account",
            "create",
            &name,
            "--expires-in",
            "12h",
            "--vault",
            "Agents:read_items",
            "--vault",
            "Dev:read_items",
            "--raw",
        ])
        .output()
        .context("could not create the 1Password service account")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        bail!("could not create the 1Password service account: {stderr}");
    }

    let mut token = output.stdout;
    while matches!(token.last(), Some(b'\n' | b'\r')) {
        token.pop();
    }
    if token.is_empty() {
        bail!("1Password did not return a service account token");
    }
    Ok(token)
}

fn install_local_op_ticket(machine: &str) -> Result<()> {
    let token = mint_op_ticket(machine)?;
    write_local_ticket(&local_ticket_path()?, &token)?;
    ensure_zshenv(&home_zshenv()?)
}

fn install_remote_op_ticket(machine: &str) -> Result<()> {
    let token = mint_op_ticket(machine)?;
    ssh_with_input(machine, REMOTE_WRITE_TICKET, &token)
        .context("could not install the remote 1Password ticket")?;
    ensure_remote_zshenv(machine)
}

fn write_local_ticket(path: &Path, token: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("1Password ticket path has no parent")?;
    let mut file = NamedTempFile::new_in(parent).with_context(|| {
        format!(
            "could not prepare the 1Password ticket at {}",
            path.display()
        )
    })?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(token)?;
    file.flush()?;
    file.as_file().sync_all()?;
    file.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("could not write the 1Password ticket at {}", path.display()))?;
    Ok(())
}

struct TempFileSignalCleanup {
    handle: SignalHandle,
    thread: Option<JoinHandle<()>>,
}

impl TempFileSignalCleanup {
    fn new(path: &Path) -> Result<Self> {
        let mut signals = Signals::new([SIGHUP, SIGINT, SIGTERM])
            .context("could not register temporary-file signal cleanup")?;
        let handle = signals.handle();
        let path = path.to_owned();
        let thread = thread::spawn(move || {
            if let Some(signal) = signals.forever().next() {
                let _ = fs::remove_file(path);
                let _ = low_level::emulate_default_handler(signal);
            }
        });
        Ok(Self {
            handle,
            thread: Some(thread),
        })
    }
}

impl Drop for TempFileSignalCleanup {
    fn drop(&mut self) {
        self.handle.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn local_op_ticket_active(path: &Path) -> bool {
    let Ok(token) = fs::read(path) else {
        return false;
    };
    if token.is_empty() {
        return false;
    }
    quiet_status_with_env("op", &["whoami"], "OP_SERVICE_ACCOUNT_TOKEN", token).unwrap_or(false)
}

fn print_status(sudo_active: bool, ticket_exists: bool, op_active: bool) {
    println!(
        "{} sudo ticket {}",
        if sudo_active { "✓" } else { "✗" },
        if sudo_active { "active" } else { "inactive" }
    );
    let op_state = if op_active {
        "active"
    } else if ticket_exists {
        "inactive (file present but rejected)"
    } else {
        "inactive (no ticket file)"
    };
    println!(
        "{} 1Password ticket {op_state}",
        if op_active { "✓" } else { "✗" }
    );
}

fn print_revoke_note() {
    println!(
        "Note: the 1Password service account lives until expiry; revoke it in the 1Password app if needed"
    );
}

fn local_ticket_path() -> Result<PathBuf> {
    if let Some(directory) = env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(directory).join("op-ticket"));
    }
    let uid = command_text(ID, &["-u"])?;
    Ok(PathBuf::from(format!("/run/user/{uid}/op-ticket")))
}

fn home_zshenv() -> Result<PathBuf> {
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".zshenv"))
}

/// Ensure the managed 1Password ticket block occurs once in a zshenv file.
pub fn ensure_zshenv(path: &Path) -> Result<()> {
    let contents = read_optional_text(path)?;
    let updated = with_managed_block(contents)?;
    write_config(path, updated.as_bytes())
}

fn with_managed_block(contents: String) -> Result<String> {
    let mut updated = strip_managed_blocks(contents)?;
    if !updated.is_empty() {
        if !updated.ends_with('\n') {
            updated.push('\n');
        }
        if !updated.ends_with("\n\n") {
            updated.push('\n');
        }
    }
    updated.push_str(ZSHENV_BLOCK);
    Ok(updated)
}

/// Remove the managed 1Password ticket block from a zshenv file.
pub fn remove_zshenv(path: &Path) -> Result<()> {
    let Some(contents) = read_existing_text(path)? else {
        return Ok(());
    };
    let updated = strip_managed_blocks(contents.clone())?;
    if updated == contents {
        return Ok(());
    }
    write_config(path, updated.as_bytes())
}

fn strip_managed_blocks(mut contents: String) -> Result<String> {
    while let Some(start) = contents.find(BLOCK_START) {
        let remainder = &contents[start + BLOCK_START.len()..];
        let relative_end = remainder
            .find(BLOCK_END)
            .context("the managed agents op-ticket block has no end marker")?;
        let mut range_start = start;
        if contents[..start].ends_with("\n\n") {
            range_start -= 1;
        }
        let mut range_end = start + BLOCK_START.len() + relative_end + BLOCK_END.len();
        if contents[range_end..].starts_with("\r\n") {
            range_end += 2;
        } else if contents[range_end..].starts_with('\n') {
            range_end += 1;
        }
        contents.replace_range(range_start..range_end, "");
    }
    Ok(contents)
}

fn ensure_remote_zshenv(machine: &str) -> Result<()> {
    let contents = marked_remote_output(ssh_output(machine, REMOTE_READ_ZSHENV)?)?;
    let text = String::from_utf8(contents).context("remote .zshenv is not valid UTF-8")?;
    let updated = with_managed_block(text)?;
    ssh_with_input(machine, REMOTE_WRITE_ZSHENV, updated.as_bytes())
        .context("could not update the remote .zshenv")
}

fn remove_remote_zshenv(machine: &str) -> Result<()> {
    if !ssh_status(machine, REMOTE_ZSHENV_EXISTS)? {
        return Ok(());
    }
    let contents = marked_remote_output(ssh_output(machine, REMOTE_READ_ZSHENV)?)?;
    let text = String::from_utf8(contents).context("remote .zshenv is not valid UTF-8")?;
    let updated = strip_managed_blocks(text.clone())?;
    if updated == text {
        return Ok(());
    }
    ssh_with_input(machine, REMOTE_WRITE_ZSHENV, updated.as_bytes())
        .context("could not update the remote .zshenv")
}

fn require_remote_target(machine: &str) -> Result<()> {
    let identity = marked_remote_output(ssh_output(machine, REMOTE_IDENTITY)?)?;
    let identity = String::from_utf8(identity).context("remote identity is not valid UTF-8")?;
    let mut lines = identity.lines();
    let os_name = lines
        .next()
        .context("remote operating system is unavailable")?;
    let uid = lines.next().context("remote user id is unavailable")?;
    require_linux(os_name)?;
    if uid == "0" {
        bail!("run this command as a normal user on {machine}, without sudo");
    }
    Ok(())
}

fn marked_remote_output(output: Vec<u8>) -> Result<Vec<u8>> {
    let marker = output
        .windows(REMOTE_OUTPUT_MARKER.len())
        .position(|window| window == REMOTE_OUTPUT_MARKER)
        .context("remote output marker is missing")?;
    Ok(output[marker + REMOTE_OUTPUT_MARKER.len()..].to_vec())
}

fn read_optional_text(path: &Path) -> Result<String> {
    Ok(read_existing_text(path)?.unwrap_or_default())
}

fn read_existing_text(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("could not read {}", path.display())),
    }
}

fn write_config(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().context("configuration path has no parent")?;
    let original_permissions = fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions());
    let mut temporary = NamedTempFile::new_in(parent)
        .with_context(|| format!("could not prepare {}", path.display()))?;
    temporary.write_all(contents)?;
    temporary.flush()?;
    if let Some(permissions) = original_permissions {
        temporary.as_file().set_permissions(permissions)?;
    } else {
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("could not write {}", path.display()))?;
    Ok(())
}

fn ssh_checked(machine: &str, script: &str) -> Result<()> {
    if ssh_status(machine, script)? {
        Ok(())
    } else {
        bail!("remote command failed on {machine}")
    }
}

fn ssh_status(machine: &str, script: &str) -> Result<bool> {
    let status = Command::new("ssh")
        .arg(machine)
        .arg(script)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("could not connect to {machine} with ssh"))?;
    if status.code() == Some(255) {
        bail!("could not connect to {machine}; run `ssh {machine}` to check access");
    }
    Ok(status.success())
}

fn ssh_output(machine: &str, script: &str) -> Result<Vec<u8>> {
    let output = Command::new("ssh")
        .arg(machine)
        .arg(script)
        .output()
        .with_context(|| format!("could not connect to {machine} with ssh"))?;
    if !output.status.success() {
        if output.status.code() == Some(255) {
            bail!("could not connect to {machine}; run `ssh {machine}` to check access");
        }
        bail!("remote command failed on {machine}");
    }
    Ok(output.stdout)
}

fn ssh_with_input(machine: &str, script: &str, input: &[u8]) -> Result<()> {
    let mut child = Command::new("ssh")
        .arg(machine)
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .with_context(|| format!("could not connect to {machine} with ssh"))?;
    child
        .stdin
        .take()
        .context("ssh input is unavailable")?
        .write_all(input)?;
    let status = child.wait()?;
    if !status.success() {
        if status.code() == Some(255) {
            bail!("could not connect to {machine}; run `ssh {machine}` to check access");
        }
        bail!("remote command failed on {machine}");
    }
    Ok(())
}

fn remove_file_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("could not remove {}", path.display())),
    }
}

fn require_normal_user() -> Result<()> {
    if command_text(ID, &["-u"])? == "0" {
        bail!("run this command as your normal user, without sudo");
    }
    Ok(())
}

/// Reject operating systems that cannot use the managed sudoers rule.
pub fn require_linux(os_name: &str) -> Result<()> {
    if os_name.trim() != "Linux" {
        bail!("Linux is required on the target machine");
    }
    Ok(())
}

/// Validate a user name before placing it in a sudoers rule.
pub fn validate_user_name(user: &str) -> Result<()> {
    if user.is_empty()
        || !user
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
    {
        bail!("unsupported user name: {user}");
    }
    Ok(())
}

/// Validate a machine name before passing it to ssh or 1Password.
pub fn validate_machine_name(machine: &str) -> Result<()> {
    if machine.is_empty()
        || !machine
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
        || machine.starts_with('-')
    {
        bail!("unsupported machine name: {machine}");
    }
    Ok(())
}

fn command_text(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("could not run {program}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        bail!("{program} failed: {stderr}");
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn checked_status_quiet_stdout(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .status()
        .with_context(|| format!("could not run {program}"))?;
    if !status.success() {
        bail!("{program} exited with {status}");
    }
    Ok(())
}

fn checked_status(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("could not run {program}"))?;
    if !status.success() {
        bail!("{program} exited with {status}");
    }
    Ok(())
}

fn quiet_status(program: &str, args: &[&str]) -> Result<bool> {
    let status = Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("could not run {program}"))?;
    Ok(status.success())
}

fn quiet_op_status(args: &[&str]) -> Result<bool> {
    let status = op_command()
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("could not run op")?;
    Ok(status.success())
}

fn op_command() -> Command {
    let mut command = Command::new("op");
    // An old agent ticket must not replace the issuing user's 1Password session.
    command.env_remove("OP_SERVICE_ACCOUNT_TOKEN");
    command
}

fn quiet_status_with_env(program: &str, args: &[&str], key: &str, value: Vec<u8>) -> Result<bool> {
    let status = Command::new(program)
        .args(args)
        .env(key, OsString::from(String::from_utf8(value)?))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("could not run {program}"))?;
    Ok(status.success())
}

#[cfg(test)]
mod tests {
    use super::sudoers_rule;

    #[test]
    fn sudoers_rule_has_the_global_twelve_hour_timeout() {
        assert_eq!(
            sudoers_rule("tom"),
            "Defaults:tom timestamp_type=global, timestamp_timeout=720"
        );
    }
}
