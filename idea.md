use nix::unistd::{fork, ForkResult, Gid, Uid, setgid, setuid, execvp};
use pam::Authenticator;
use std::ffi::CString;
use std::process::exit;
use users::{get_user_by_name, User};

fn dummy_conversation(_messages: Vec<pam::module::Message>) -> Result<Vec<pam::module::Response>, pam::constants::PamError> {
    Ok(vec![])
}

fn main() {
    let username = "targetuser";

    // 1. Resolve user profile directly from the system (/etc/passwd)
    let user: User = get_user_by_name(username).expect("User not found on system");
    let target_uid = Uid::from_raw(user.uid());
    let target_gid = Gid::from_raw(user.gid());
    
    // Fallback to /bin/sh if no shell is assigned
    let user_shell = user.shell().to_str().unwrap_or("/bin/bash").to_string(); 
    let user_home = user.home_dir().to_str().unwrap_or("/home/targetuser").to_string();

    // 2. Initialize and open the PAM context
    let mut auth = Authenticator::with_handler("systemd-user", username, dummy_conversation)
        .expect("Failed to initialize PAM");
    auth.open_session().expect("PAM session failed to open");

    match unsafe { fork() } {
        Ok(ForkResult::Parent { child: _ }) => {
            // Parent blocks or tracks session
        }
        Ok(ForkResult::Child) => {
            // --- INSIDE CHILD PROCESS ---

            // 3. Scrub parent (Root Daemon) environment completely to prevent leakage
            std::env::vars().for_each(|(key, _)| std::env::remove_var(key));

            // 4. Inject standard base environment invariants required for a login
            std::env::set_var("USER", username);
            std::env::set_var("LOGNAME", username);
            std::env::set_var("HOME", &user_home);
            std::env::set_var("SHELL", &user_shell);
            
            // Your custom isolated remote desktop environment overrides
            let runtime_dir = format!("/run/user/remote-{}", user.uid());
            std::env::set_var("XDG_RUNTIME_DIR", &runtime_dir);
            std::env::set_var("DISPLAY", ":10"); 

            // 5. Extract and apply variables initialized by PAM modules (e.g., pam_env.so)
            // This safely grabs things like localized variables, global paths, and security tokens
            for env_string in auth.envlist() {
                if let Some((key, value)) = env_string.split_once('=') {
                    // Do not let PAM overwrite your isolated XDG_RUNTIME_DIR or core paths
                    if key != "XDG_RUNTIME_DIR" && key != "HOME" {
                        std::env::set_var(key, value);
                    }
                }
            }

            // 6. Permanently drop root privileges to the target user
            setgid(target_gid).expect("Failed to drop GID");
            setuid(target_uid).expect("Failed to drop UID");

            // 7. Force the user's shell to execute as an interactive login shell (-l)
            // By calling "dbus-run-session" wrapping "bash -l -c 'your-desktop-command'",
            // the user's login shell natively parses /etc/profile, ~/.bash_profile, etc.
            let desktop_cmd = "startxfce4"; // Your desktop startup hook
            let shell_payload = format!("exec {} -l -c '{}'", user_shell, desktop_cmd);

            let program = CString::new("dbus-run-session").unwrap();
            let args = vec![
                CString::new("dbus-run-session").unwrap(),
                CString::new("--").unwrap(),
                CString::new("sh").unwrap(),
                CString::new("-c").unwrap(),
                CString::new(shell_payload).unwrap(),
            ];

            // 8. Hand off control to the login shell execution chain
            execvp(&program, &args).expect("Fatal error executing session login chain");
            exit(1);
        }
        Err(_) => exit(1),
    }
}

