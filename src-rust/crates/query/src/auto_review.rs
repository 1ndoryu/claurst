use claurst_api::client::ClientConfig;
use claurst_api::AnthropicClient;
use claurst_core::constants::{TOOL_NAME_FILE_READ, TOOL_NAME_GLOB, TOOL_NAME_GREP};
use claurst_core::file_history::TurnFileSnapshot;
use claurst_core::tasks::{global_registry, BackgroundTask, TaskStatus};
use claurst_core::types::Message;
use claurst_tools::{all_tools, Tool, ToolContext};
use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::{run_query_loop, QueryConfig, QueryOutcome};

const MAX_REVIEWABLE_FILES: usize = 8;
const CHANGE_CONTEXT_LINES: usize = 6;
const MAX_CHANGE_EXCERPT_CHARS: usize = 2_400;
const MAX_NOTIFICATION_CHARS: usize = 3_200;
const AUTO_REVIEW_SYSTEM_PROMPT: &str = "You are a background code review agent. Review only the provided code changes and the current files they reference. Ignore style nits, speculative advice, and anything not grounded in the changed code. If this is a false positive, a docs-only change, or there is no actionable issue, respond exactly as `NO_REVIEW_NEEDED: <one sentence>`. Otherwise respond with terse findings only, each including severity, file path, and the concrete risk. Do not suggest writing code or running write tools.";

struct ReviewArea {
    id: &'static str,
    label: &'static str,
    focus: &'static str,
}

const REVIEW_AREAS: [ReviewArea; 3] = [
    ReviewArea {
        id: "correctness",
        label: "Correctness",
        focus: "Look for behavioral regressions, broken control flow, edge cases introduced by the change, and mismatches between the apparent intent and the final code.",
    },
    ReviewArea {
        id: "security",
        label: "Security",
        focus: "Look for unsafe input handling, auth/permission gaps, injection risks, unsafe filesystem or process usage, secret exposure, and missing validation at boundaries.",
    },
    ReviewArea {
        id: "architecture",
        label: "Architecture",
        focus: "Look for violations of local architecture boundaries, missing validation/tests around the changed slice, surprising coupling, or changes that are harder to maintain than necessary.",
    },
];

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReviewableChange {
    relative_path: String,
    excerpt: String,
    executable: bool,
}

pub fn maybe_spawn_auto_review(tool_ctx: &ToolContext, config: &QueryConfig) {
    if !tool_ctx.config.auto_review_enabled() {
        debug!("Auto-review skipped: disabled by config");
        return;
    }

    let Some(notifier) = tool_ctx.completion_notifier.clone() else {
        debug!("Auto-review skipped: no completion notifier wired");
        return;
    };

    let turn_index = tool_ctx.current_turn_index();
    let changes = {
        let history = tool_ctx.file_history.lock();
        collect_reviewable_changes(
            &history.snapshots_for_turn(turn_index),
            &tool_ctx.working_dir,
        )
    };

    if changes.is_empty() {
        return;
    }

    for area in selected_review_areas(&changes) {
        spawn_review_agent(area, changes.clone(), tool_ctx, config, notifier.clone());
    }
}

fn selected_review_areas(changes: &[ReviewableChange]) -> Vec<&'static ReviewArea> {
    let mut areas = vec![&REVIEW_AREAS[0], &REVIEW_AREAS[2]];
    if changes.iter().any(|change| change.executable) {
        areas.insert(1, &REVIEW_AREAS[1]);
    }
    areas
}

fn spawn_review_agent(
    area: &'static ReviewArea,
    changes: Vec<ReviewableChange>,
    tool_ctx: &ToolContext,
    config: &QueryConfig,
    notifier: claurst_tools::CompletionNotifier,
) {
    let mut task = BackgroundTask::new(format!("auto-review: {}", area.label));
    let task_id = task.id.clone();
    task.name = format!("auto-review {}", area.id);
    global_registry().register(task);

    let review_ctx = isolated_review_context(tool_ctx);
    let review_config = review_query_config(tool_ctx, config, area);
    let prompt = build_review_prompt(area, &changes);

    tokio::spawn(async move {
        let client = match AnthropicClient::new(ClientConfig {
            api_key: review_ctx
                .config
                .resolve_anthropic_api_key()
                .unwrap_or_default(),
            api_base: review_ctx.config.resolve_anthropic_api_base(),
            ..Default::default()
        }) {
            Ok(client) => client,
            Err(err) => {
                global_registry().update_status(
                    &task_id,
                    TaskStatus::Failed(format!("client init failed: {}", err)),
                );
                warn!(area = area.id, error = %err, "Auto-review client init failed");
                return;
            }
        };

        let review_tools = build_review_tools();
        let mut messages = vec![Message::user(prompt)];
        let outcome = run_query_loop(
            &client,
            &mut messages,
            &review_tools,
            &review_ctx,
            &review_config,
            review_ctx.cost_tracker.clone(),
            None,
            CancellationToken::new(),
            None,
        )
        .await;

        let report = outcome_to_text(outcome);
        let status = if report.starts_with("[Agent error:") || report.starts_with("[Agent stopped:")
        {
            TaskStatus::Failed(report.clone())
        } else {
            TaskStatus::Completed
        };
        global_registry().append_output(&task_id, &report);
        global_registry().update_status(&task_id, status);

        if let Some(note) = build_completion_notification(area, &changes, &report) {
            notifier.notify(note);
        }

        debug!(
            area = area.id,
            task_id = %task_id,
            changed_files = changes.len(),
            "Auto-review agent completed"
        );
    });
}

fn isolated_review_context(tool_ctx: &ToolContext) -> ToolContext {
    let mut review_ctx = tool_ctx.clone();
    review_ctx.file_history = Arc::new(parking_lot::Mutex::new(
        claurst_core::file_history::FileHistory::new(),
    ));
    review_ctx.current_turn = Arc::new(AtomicUsize::new(0));
    review_ctx.completion_notifier = None;
    review_ctx.pending_permissions = None;
    review_ctx.user_question_tx = None;
    review_ctx
}

fn review_query_config(
    tool_ctx: &ToolContext,
    config: &QueryConfig,
    area: &ReviewArea,
) -> QueryConfig {
    let model = tool_ctx.config.effective_subagent_model().to_string();

    QueryConfig {
        model,
        max_tokens: config.max_tokens.min(4_096),
        max_turns: 6,
        system_prompt: Some(format!(
            "{}\n\nReview focus: {}",
            AUTO_REVIEW_SYSTEM_PROMPT, area.focus
        )),
        append_system_prompt: None,
        output_style: config.output_style.clone(),
        output_style_prompt: config.output_style_prompt.clone(),
        working_directory: Some(tool_ctx.working_dir.display().to_string()),
        thinking_budget: config.thinking_budget,
        temperature: config.temperature,
        tool_result_budget: config.tool_result_budget.min(20_000),
        effort_level: config.effort_level.clone(),
        command_queue: None,
        skill_index: None,
        max_budget_usd: config.max_budget_usd,
        fallback_model: None,
        provider_registry: config.provider_registry.clone(),
        agent_name: Some(format!("auto-review-{}", area.id)),
        agent_definition: None,
        model_registry: config.model_registry.clone(),
        managed_agents: None,
    }
}

fn build_review_tools() -> Vec<Box<dyn Tool>> {
    let allowed = [TOOL_NAME_FILE_READ, TOOL_NAME_GLOB, TOOL_NAME_GREP];
    all_tools()
        .into_iter()
        .filter(|tool| allowed.contains(&tool.name()))
        .collect()
}

fn build_review_prompt(area: &ReviewArea, changes: &[ReviewableChange]) -> String {
    let mut prompt = String::from(
        "Review the following code changes. You may inspect the current files with read-only tools before deciding. If there is no real issue, respond with NO_REVIEW_NEEDED exactly as instructed.\n\n",
    );
    prompt.push_str("Focus area: ");
    prompt.push_str(area.label);
    prompt.push_str("\n");
    prompt.push_str(area.focus);
    prompt.push_str("\n\nChanged files:\n");

    for change in changes {
        prompt.push_str("- ");
        prompt.push_str(&change.relative_path);
        prompt.push('\n');
    }

    prompt.push_str("\nChange excerpts:\n\n");
    for change in changes {
        prompt.push_str("File: ");
        prompt.push_str(&change.relative_path);
        prompt.push('\n');
        prompt.push_str(&change.excerpt);
        prompt.push_str("\n\n");
    }

    prompt.push_str(
        "Response rules:\n- Use only findings grounded in these changes and the current files.\n- Prefer real regressions or risks over speculative nits.\n- Keep the response concise.\n",
    );
    prompt
}

fn build_completion_notification(
    area: &ReviewArea,
    changes: &[ReviewableChange],
    report: &str,
) -> Option<String> {
    let trimmed = report.trim();
    if trimmed.is_empty() || trimmed.starts_with("NO_REVIEW_NEEDED:") {
        return None;
    }

    let mut note = format!(
        "[AutoReview/{}] Findings for {}.\n\n{}\n\nDecide whether these findings are worth applying; ignore this note if the concern is already addressed.",
        area.id,
        summarize_paths(changes),
        trimmed,
    );

    if note.chars().count() > MAX_NOTIFICATION_CHARS {
        note = truncate_chars(&note, MAX_NOTIFICATION_CHARS);
        note.push_str("\n\n[AutoReview truncated]");
    }

    Some(note)
}

fn summarize_paths(changes: &[ReviewableChange]) -> String {
    let mut paths: Vec<&str> = changes
        .iter()
        .map(|change| change.relative_path.as_str())
        .collect();
    paths.sort_unstable();
    let preview: Vec<&str> = paths.into_iter().take(4).collect();
    if changes.len() > preview.len() {
        format!(
            "{} and {} more file(s)",
            preview.join(", "),
            changes.len() - preview.len()
        )
    } else {
        preview.join(", ")
    }
}

fn outcome_to_text(outcome: QueryOutcome) -> String {
    match outcome {
        QueryOutcome::EndTurn { message, .. } => message.get_all_text(),
        QueryOutcome::MaxTokens {
            partial_message, ..
        } => format!(
            "{}\n\n[Note: Auto-review hit max_tokens limit]",
            partial_message.get_all_text()
        ),
        QueryOutcome::Cancelled => "[Agent stopped: auto-review was cancelled]".to_string(),
        QueryOutcome::Error(err) => format!("[Agent error: {}]", err),
        QueryOutcome::BudgetExceeded {
            cost_usd,
            limit_usd,
        } => format!(
            "[Agent stopped: auto-review exceeded budget ${:.4} / ${:.4}]",
            cost_usd, limit_usd
        ),
    }
}

fn collect_reviewable_changes(
    snapshots: &[TurnFileSnapshot],
    working_dir: &Path,
) -> Vec<ReviewableChange> {
    snapshots
        .iter()
        .filter_map(|snapshot| reviewable_change(snapshot, working_dir))
        .take(MAX_REVIEWABLE_FILES)
        .collect()
}

fn reviewable_change(snapshot: &TurnFileSnapshot, working_dir: &Path) -> Option<ReviewableChange> {
    if snapshot.binary || !is_reviewable_path(&snapshot.path) {
        return None;
    }

    let before = snapshot.before_text.as_deref().unwrap_or("");
    let after = snapshot.after_text.as_deref().unwrap_or("");
    if normalize_for_meaningful_change(before) == normalize_for_meaningful_change(after) {
        return None;
    }

    let relative_path = snapshot
        .path
        .strip_prefix(working_dir)
        .unwrap_or(snapshot.path.as_path())
        .to_string_lossy()
        .replace('\\', "/");

    Some(ReviewableChange {
        relative_path,
        excerpt: build_change_excerpt(before, after),
        executable: is_executable_review_path(&snapshot.path),
    })
}

fn is_reviewable_path(path: &Path) -> bool {
    let lower = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    for ignored in [
        "/node_modules/",
        "/target/",
        "/dist/",
        "/build/",
        "/coverage/",
        "/vendor/",
        "/.git/",
    ] {
        if lower.contains(ignored) {
            return false;
        }
    }

    if lower.ends_with("cargo.lock")
        || lower.ends_with("package-lock.json")
        || lower.ends_with("pnpm-lock.yaml")
        || lower.ends_with("yarn.lock")
    {
        return false;
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(
        file_name.as_str(),
        "dockerfile" | "makefile" | "justfile" | "cargo.toml" | "package.json" | "composer.json"
    ) {
        return true;
    }

    matches!(
        path.extension().and_then(|ext| ext.to_str()).map(|ext| ext.to_ascii_lowercase()),
        Some(ext)
            if matches!(
                ext.as_str(),
                "rs"
                    | "ts"
                    | "tsx"
                    | "js"
                    | "jsx"
                    | "mjs"
                    | "cjs"
                    | "py"
                    | "php"
                    | "go"
                    | "java"
                    | "kt"
                    | "kts"
                    | "swift"
                    | "rb"
                    | "sh"
                    | "ps1"
                    | "sql"
                    | "toml"
                    | "yml"
                    | "yaml"
                    | "css"
                    | "scss"
                    | "html"
                    | "htm"
            )
    )
}

fn is_executable_review_path(path: &Path) -> bool {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(
        file_name.as_str(),
        "dockerfile" | "makefile" | "justfile" | "cargo.toml" | "package.json" | "composer.json"
    ) {
        return true;
    }

    matches!(
        path.extension().and_then(|ext| ext.to_str()).map(|ext| ext.to_ascii_lowercase()),
        Some(ext)
            if matches!(
                ext.as_str(),
                "rs"
                    | "ts"
                    | "tsx"
                    | "js"
                    | "jsx"
                    | "mjs"
                    | "cjs"
                    | "py"
                    | "php"
                    | "go"
                    | "java"
                    | "kt"
                    | "kts"
                    | "swift"
                    | "rb"
                    | "sh"
                    | "ps1"
                    | "sql"
                    | "toml"
                    | "yml"
                    | "yaml"
            )
    )
}

fn normalize_for_meaningful_change(input: &str) -> String {
    input
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn build_change_excerpt(before: &str, after: &str) -> String {
    let before_lines: Vec<&str> = before.lines().collect();
    let after_lines: Vec<&str> = after.lines().collect();
    let common_prefix = before_lines
        .iter()
        .zip(after_lines.iter())
        .take_while(|(left, right)| left == right)
        .count();

    let mut common_suffix = 0usize;
    while common_suffix < before_lines.len().saturating_sub(common_prefix)
        && common_suffix < after_lines.len().saturating_sub(common_prefix)
        && before_lines[before_lines.len() - 1 - common_suffix]
            == after_lines[after_lines.len() - 1 - common_suffix]
    {
        common_suffix += 1;
    }

    let before_start = common_prefix.saturating_sub(CHANGE_CONTEXT_LINES);
    let after_start = common_prefix.saturating_sub(CHANGE_CONTEXT_LINES);
    let before_end = (before_lines.len().saturating_sub(common_suffix) + CHANGE_CONTEXT_LINES)
        .min(before_lines.len());
    let after_end = (after_lines.len().saturating_sub(common_suffix) + CHANGE_CONTEXT_LINES)
        .min(after_lines.len());

    let before_block = render_excerpt_block(
        &before_lines,
        before_start,
        before_end,
        before_start > 0,
        before_end < before_lines.len(),
    );
    let after_block = render_excerpt_block(
        &after_lines,
        after_start,
        after_end,
        after_start > 0,
        after_end < after_lines.len(),
    );

    let mut excerpt = format!(
        "Change window: before lines {}-{}, after lines {}-{}\n--- before\n{}\n--- after\n{}",
        line_range_start(before_start),
        line_range_end(before_end),
        line_range_start(after_start),
        line_range_end(after_end),
        before_block,
        after_block,
    );

    if excerpt.chars().count() > MAX_CHANGE_EXCERPT_CHARS {
        excerpt = truncate_chars(&excerpt, MAX_CHANGE_EXCERPT_CHARS);
        excerpt.push_str("\n[excerpt truncated]");
    }

    excerpt
}

fn render_excerpt_block(
    lines: &[&str],
    start: usize,
    end: usize,
    has_prefix_gap: bool,
    has_suffix_gap: bool,
) -> String {
    let mut block = String::new();
    if has_prefix_gap {
        block.push_str("...\n");
    }
    for line in &lines[start..end] {
        block.push_str(line);
        block.push('\n');
    }
    if has_suffix_gap {
        block.push_str("...");
    }
    block.trim_end_matches('\n').to_string()
}

fn line_range_start(index: usize) -> usize {
    if index == 0 {
        1
    } else {
        index + 1
    }
}

fn line_range_end(index: usize) -> usize {
    index.max(1)
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    input.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn skips_non_code_and_whitespace_only_changes() {
        let working_dir = Path::new("/repo");
        let snapshots = vec![
            TurnFileSnapshot {
                path: PathBuf::from("/repo/docs/readme.md"),
                before_text: Some("old".to_string()),
                after_text: Some("new".to_string()),
                binary: false,
                turn_index: 2,
            },
            TurnFileSnapshot {
                path: PathBuf::from("/repo/src/main.rs"),
                before_text: Some("fn main() {\n    println!(\"x\");\n}\n".to_string()),
                after_text: Some("fn main() {\n\n    println!(\"x\");\n}\n".to_string()),
                binary: false,
                turn_index: 2,
            },
        ];

        let changes = collect_reviewable_changes(&snapshots, working_dir);
        assert!(changes.is_empty());
    }

    #[test]
    fn keeps_reviewable_code_changes() {
        let working_dir = Path::new("/repo");
        let snapshots = vec![TurnFileSnapshot {
            path: PathBuf::from("/repo/src/main.rs"),
            before_text: Some("fn main() {\n    do_a();\n}\n".to_string()),
            after_text: Some("fn main() {\n    do_a();\n    do_b();\n}\n".to_string()),
            binary: false,
            turn_index: 2,
        }];

        let changes = collect_reviewable_changes(&snapshots, working_dir);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].relative_path, "src/main.rs");
        assert!(changes[0].excerpt.contains("do_b"));
        assert!(changes[0].executable);
    }

    #[test]
    fn suppresses_false_positive_notifications() {
        let changes = vec![ReviewableChange {
            relative_path: "src/main.rs".to_string(),
            excerpt: "...".to_string(),
            executable: true,
        }];

        assert!(build_completion_notification(
            &REVIEW_AREAS[0],
            &changes,
            "NO_REVIEW_NEEDED: formatting only"
        )
        .is_none());
    }
}
