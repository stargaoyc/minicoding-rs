//! `minicoding doctor --security` 子命令（T-M4-10）。
//!
//! 自检沙箱驱动类型、硬化状态、VCS 保护、权限配置，输出人类可读报告。
//! 不构建 `Runtime`，仅调用 `minicoding-sandbox` 的探测函数。
//!
//! ## 输出示例
//!
//! ```text
//! minicoding security doctor
//! ─────────────────────────────
//! platform:        linux
//! sandbox driver:  landlock
//! hardened:        yes
//! vcs protected:   .git, .hg, .svn
//! landlock abi:    V3 (Linux 6.2+)
//! ```

use clap::Args;

/// `doctor` 子命令选项。
#[derive(Args, Debug)]
pub struct DoctorCommand {
    /// 仅输出安全自检（沙箱驱动/硬化/VCS 保护）。
    #[arg(long)]
    pub security: bool,
}

/// 执行 `doctor` 子命令。
pub fn run_doctor_command(cmd: &DoctorCommand) {
    if cmd.security {
        print_security_report();
    } else {
        // 无 `--security` 时打印整体诊断（M4 仅交付 security 子项）
        println!("minicoding doctor");
        println!("─────────────────────");
        print_security_report();
    }
}

/// 打印安全自检报告。
fn print_security_report() {
    let platform = current_platform();

    #[cfg(feature = "sandbox")]
    {
        let kind = minicoding_sandbox::detect_driver_kind();
        let driver = minicoding_sandbox::detect_driver();
        let hardened = driver.is_hardened();

        println!("platform:        {platform}");
        println!("sandbox driver:  {}", kind.as_str());
        println!("hardened:        {}", if hardened { "yes" } else { "no" });

        // VCS 保护目录（应用层 builtin 黑名单补充，见 security.md §3）
        let vcs_dirs = minicoding_sandbox::vcs_protected_dirs(std::path::Path::new("."));
        let vcs_list = vcs_dirs
            .iter()
            .filter_map(|p| p.file_name())
            .map(|s| s.to_string_lossy())
            .collect::<Vec<_>>()
            .join(", ");
        println!("vcs protected:   {vcs_list}");

        // Landlock 特定信息（仅 Linux）：通过 driver kind 判断可用性
        #[cfg(target_os = "linux")]
        {
            let abi = if matches!(kind, minicoding_sandbox::DriverKind::Landlock) {
                "V3 (Linux 6.2+, target ABI)"
            } else {
                "unavailable (kernel < 5.13 or disabled)"
            };
            println!("landlock abi:    {abi}");
        }

        if !hardened {
            println!();
            println!("warning: 沙箱未硬化，副作用工具仅受应用层权限约束（C-22）。");
            println!("         在 CI/容器内可用 --preset external-sandbox 显式声明外部隔离。");
        }
    }

    #[cfg(not(feature = "sandbox"))]
    {
        println!("platform:        {platform}");
        println!("sandbox driver:  (disabled, rebuild with --features sandbox)");
        println!("hardened:        no");
    }
}

/// 当前平台字符串。
fn current_platform() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        "linux"
    }
    #[cfg(target_os = "macos")]
    {
        "macos"
    }
    #[cfg(target_os = "windows")]
    {
        "windows"
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        "unknown"
    }
}
