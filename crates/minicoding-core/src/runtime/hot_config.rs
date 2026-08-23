//! turn 边界白名单配置热更新（A-2026-08 自 rt.rs 抽出；M-12/R-04/S-22，
//! 见 `tech-stack.md` §13 决策记录）。
//!
//! [`ConfigWatcher`] 仅探测变更并广播 `Event::ConfigChanged`，具体应用由
//! [`Runtime::reload_safe_config`] 在每次 `run_turn` 开头执行：
//! - **不做全量热重载**（C-29 压缩熔断状态机与 provider 依赖构造时配置）；
//! - 白名单字段仅当文件中显式存在该 key 时应用；
//! - 显式覆盖保护（2026-08-23 审查 §3-P1）：运行期值偏离热更新基线说明调用方
//!   经 CLI flag/env 显式覆盖过，文件值不得回退覆盖。

use super::rt::Runtime;
use crate::config::RuntimeConfig;

/// 热更新基线（白名单字段的"最近一次非覆盖来源"值）。
///
/// 构造时快照 `RuntimeConfig` 的白名单字段；`reload_safe_config` 仅当运行期值 ==
/// 基线时才应用文件值——若调用方经 CLI flag/env 显式覆盖过（运行期值偏离基线），
/// 文件值不得回退覆盖，维持"CLI 参数 > 环境变量 > config.toml > 默认"的文档优先级。
/// 每次成功应用文件值后基线随之**滚动前移**（否则首次应用即永久阻断后续更新）。
/// Mutex 仅为内部可变性：临界区无 await。
#[derive(Debug)]
pub(crate) struct HotReloadBaseline {
    model: String,
    turn_timeout_sec: u64,
    parallel_reads: u32,
}

impl HotReloadBaseline {
    pub(crate) fn capture(config: &RuntimeConfig) -> std::sync::Mutex<Self> {
        std::sync::Mutex::new(Self {
            model: config.provider.model.clone(),
            turn_timeout_sec: config.context.turn_timeout_sec,
            parallel_reads: config.tools.parallel_reads,
        })
    }
}

impl Runtime {
    /// M-12（R-04）：turn 边界白名单配置热更新。
    ///
    /// [`crate::config::ConfigWatcher`]（`paths::config_path()` 监听）仅广播
    /// `Event::ConfigChanged`，具体应用由本方法在 `run_turn` 开头执行
    /// （`tech-stack.md` §13 决策记录）：
    /// - **不做全量热重载**：C-29 压缩熔断状态机与 provider 重建依赖构造时配置，
    ///   热换不安全；白名单外的字段变更仅 warn 提示重启。
    /// - 白名单字段（`provider.model`/`context.turn_timeout_sec`/
    ///   `tools.parallel_reads`）**仅当文件中显式存在**该 key 时应用
    ///   （`toml::Value` presence 判断），避免 serde default（文件缺字段补默认值）
    ///   覆盖 CLI/env 传入的覆盖值。
    /// - 文件缺失/解析失败时静默保留当前配置（best-effort，与 `load_config` 的
    ///   last-known-good 机制正交：此处不写 LKG）。
    pub(crate) async fn reload_safe_config(&self) {
        let Some(path) = &self.config_path else {
            return;
        };
        let Ok(raw) = tokio::fs::read_to_string(path).await else {
            return; // 无配置文件：CLI 未配置时的正常路径，静默跳过
        };
        let fresh: RuntimeConfig = match toml::from_str(&raw) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    path = %path,
                    error = %e,
                    "config reload: parse failed, keeping current config"
                );
                return;
            }
        };
        let file_val: toml::Value = match toml::from_str(&raw) {
            Ok(v) => v,
            Err(_) => return, // 与上方同源解析，理论不可达
        };

        // 非白名单签名（白名单字段 + revision 剥除后）：
        // 先读当前运行期签名，供下方「变更提示重启」比对。
        let sig_current = {
            let cfg = self
                .config
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Self::config_non_whitelist_sig(&cfg)
        };
        let sig_fresh = Self::config_non_whitelist_sig(&fresh);

        // 应用白名单字段（写锁临界区仅字段赋值，无 await）。
        // 每个字段先比对热更新基线：运行期值偏离基线说明调用方经 CLI flag/env
        // 显式覆盖过，文件值不得回退覆盖（优先级 CLI > env > file）。
        let (base_model, base_turn_timeout, base_parallel) = {
            let baseline = self
                .hot_reload_baseline
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                baseline.model.clone(),
                baseline.turn_timeout_sec,
                baseline.parallel_reads,
            )
        };
        let mut cfg = self
            .config
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut applied: Vec<&'static str> = Vec::new();
        if Self::toml_has(&file_val, &["provider", "model"]) && cfg.provider.model == base_model {
            cfg.provider.model.clone_from(&fresh.provider.model);
            applied.push("provider.model");
        }
        if Self::toml_has(&file_val, &["context", "turn_timeout_sec"])
            && cfg.context.turn_timeout_sec == base_turn_timeout
        {
            cfg.context.turn_timeout_sec = fresh.context.turn_timeout_sec;
            applied.push("context.turn_timeout_sec");
        }
        if Self::toml_has(&file_val, &["tools", "parallel_reads"])
            && cfg.tools.parallel_reads == base_parallel
        {
            cfg.tools.parallel_reads = fresh.tools.parallel_reads;
            applied.push("tools.parallel_reads");
        }
        drop(cfg);
        // 基线滚动前移：成功应用的字段以文件值为新基线（否则首次应用即永久阻断）。
        if !applied.is_empty() {
            let mut baseline = self
                .hot_reload_baseline
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if applied.contains(&"provider.model") {
                baseline.model.clone_from(&fresh.provider.model);
            }
            if applied.contains(&"context.turn_timeout_sec") {
                baseline.turn_timeout_sec = fresh.context.turn_timeout_sec;
            }
            if applied.contains(&"tools.parallel_reads") {
                baseline.parallel_reads = fresh.tools.parallel_reads;
            }
        }

        if !applied.is_empty() {
            tracing::info!(
                path = %path,
                applied = ?applied,
                "config reload: applied whitelist fields at turn boundary"
            );
        }

        // 非白名单变更检测：与上次文件版本比对（首次加载 `last == None` 不告警）。
        let mut last = self
            .last_non_whitelist_sig
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let needs_warn = last.is_some() && sig_current != sig_fresh && *last != Some(sig_fresh);
        *last = Some(sig_fresh);
        if needs_warn {
            tracing::warn!(
                path = %path,
                "config reload: detected non-whitelist changes (restart required to take effect); whitelist fields applied"
            );
        }
    }

    /// 在 `toml::Value` 中按路径查找 key 是否存在（M-12 白名单 presence 判断）。
    fn toml_has(v: &toml::Value, path: &[&str]) -> bool {
        let mut cur = v;
        for key in path {
            match cur.get(key) {
                Some(next) => cur = next,
                None => return false,
            }
        }
        true
    }

    /// 配置的「非白名单签名」：序列化 JSON 后剔除白名单路径与 `revision`，
    /// 对剩余字符串做 hash。用于检测「需重启生效」的配置变更（M-12）。
    fn config_non_whitelist_sig(cfg: &RuntimeConfig) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut v = serde_json::to_value(cfg).unwrap_or_default();
        if let Some(o) = v.as_object_mut() {
            o.remove("revision");
        }
        if let Some(p) = v.pointer_mut("/provider")
            && let Some(o) = p.as_object_mut()
        {
            o.remove("model");
        }
        if let Some(c) = v.pointer_mut("/context")
            && let Some(o) = c.as_object_mut()
        {
            o.remove("turn_timeout_sec");
        }
        if let Some(t) = v.pointer_mut("/tools")
            && let Some(o) = t.as_object_mut()
        {
            o.remove("parallel_reads");
        }
        let mut h = DefaultHasher::new();
        v.to_string().hash(&mut h);
        h.finish()
    }
}
