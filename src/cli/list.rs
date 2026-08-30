use std::io::{self, IsTerminal};

use ratatui::prelude::{Color, Constraint, Layout, Line, Modifier, Rect, Span, Style};
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame};
// Input events are delegated to ratatui's own crossterm backend crate; its
// re-export is the ratatui-blessed way to read them (no direct crossterm dep)
use ratatui_crossterm::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use serde::Serialize;

use crate::core::current::{self, CurrentTools};
use crate::core::error::UError;
use crate::core::paths::{plugin_path, tools_dir};
use crate::toolset::{self, RemoteVersion};
use crate::ui::report::print_hint;
use crate::ui::style::{odim, ogreen};

/// Remote listings with more than this many versions open in an interactive
/// pager instead of dumping to the terminal. `--all` (or piped output) always
/// prints in full; JSON is never paged.
const PAGER_THRESHOLD: usize = 50;

/// List installed tools and versions, or a tool's remotely available versions.
///
/// Output is newest-first (human and JSON alike). Local listing marks the
/// active version with `(current)` and always prints in full. Remote listing
/// marks `(latest)` / `(lts: <codename>)` and narrows by version prefix
/// (`uvman list node --remote 22`); when more than 50 versions would be
/// printed on a terminal they open in an interactive pager (`--all` prints
/// everything directly, and piped/redirected output is never paged). JSON is
/// always full data.
#[derive(Debug, clap::Args)]
#[clap(verbatim_doc_comment, visible_alias = "ls")]
pub struct List {
    /// Tool name; omit to list every installed tool
    pub tool: Option<String>,

    /// Version prefix to filter remote versions by (requires --remote)
    #[clap(requires = "remote")]
    pub filter: Option<String>,

    /// List a tool's remotely available versions (requires TOOL)
    #[clap(short = 'r', long, requires = "tool")]
    pub remote: bool,

    /// Print the full remote list directly instead of paging (requires --remote)
    #[clap(short = 'a', long, requires = "remote")]
    pub all: bool,

    /// Emit machine-readable JSON (always full data, never truncated)
    #[clap(short = 'J', long)]
    pub json: bool,
}

impl List {
    pub async fn run(&self) -> crate::Result<()> {
        if self.remote { self.run_remote().await } else { self.run_local() }
    }

    // ---------------- Local listing ----------------

    fn run_local(&self) -> crate::Result<()> {
        match &self.tool {
            Some(name) => Ok(self.list_one_local(name)?),
            None => Ok(self.list_all_local()?),
        }
    }

    /// One tool: versions (newest first) plus the version `use` recorded as
    /// active; an uninstalled tool gets `(none)` and a next-step hint instead
    /// of a silent empty list
    fn list_one_local(&self, name: &str) -> Result<(), UError> {
        let versions = installed_desc(name);
        let currents = current::load();

        if self.json {
            let value = local_json(&[(name.to_string(), versions)], &currents);
            print_json(&value)?;
        } else {
            print_local_section(name, &versions, current_of(&currents, name));
            if versions.is_empty() {
                hint_not_installed(name);
            }
        }
        Ok(())
    }

    /// Every installed tool as its own block, sorted by name
    fn list_all_local(&self) -> Result<(), UError> {
        let tools = collect_installed_tools()?;
        let currents = current::load();

        if self.json {
            let value = local_json(&tools, &currents);
            print_json(&value)?;
        } else if tools.is_empty() {
            println!("{}", odim("no tools installed yet"));
            print_hint("install a tool to get started", &["uvman install node@lts".to_string()]);
        } else {
            for (name, versions) in &tools {
                print_local_section(name, versions, current_of(&currents, name));
            }
        }
        Ok(())
    }

    // ---------------- Remote listing ----------------

    async fn run_remote(&self) -> crate::Result<()> {
        // clap(requires = "tool") normally catches a missing tool; this is a
        // defensive fallback
        let Some(tool) = self.tool.as_deref() else {
            return Err(UError::SimpleError(
                "remote listing requires a tool name: uvman list <tool> --remote".into(),
            )
            .into());
        };

        let mut versions = toolset::remote_versions(tool).await?;
        sort_remote_desc(&mut versions);
        let currents = current::load();
        let current = currents.tools.get(tool).map(|e| e.version.clone());

        if self.json {
            let value = serde_json::json!({ "tool": tool, "versions": versions });
            print_json(&value)?;
            return Ok(());
        }

        // latest/lts are resolved on the full set so they stay correct after
        // prefix filtering
        let latest = versions.first().map(|v| v.version.clone());
        let lts = newest_lts(&versions);

        let selected: Vec<&RemoteVersion> = match &self.filter {
            Some(prefix) => {
                versions.iter().filter(|v| matches_prefix(&v.version, prefix)).collect()
            },
            None => versions.iter().collect(),
        };

        if selected.is_empty() {
            println!("{}", ogreen(format!("{tool}:")));
            match &self.filter {
                Some(prefix) => println!("{}", odim(format!("no versions match '{prefix}'"))),
                None => println!("  (none)"),
            }
            return Ok(());
        }

        let rows: Vec<VersionRow> = selected
            .iter()
            .map(|v| VersionRow {
                version: v.version.clone(),
                markers: markers_for(
                    &v.version,
                    current.as_deref(),
                    latest.as_deref(),
                    lts.as_ref().map(|(version, codename)| (version.as_str(), codename.as_str())),
                ),
            })
            .collect();

        // The user's rule: everything ≤ PAGER_THRESHOLD prints in full; more
        // than that opens an interactive pager on a terminal. --all and
        // piped/redirected output always print in full.
        if selected.len() > PAGER_THRESHOLD && !self.all && io::stdout().is_terminal() {
            show_pager(tool, &rows).unwrap_or_else(|_| print_plain(tool, &rows));
        } else {
            print_plain(tool, &rows);
        }
        Ok(())
    }
}

// ---------------- Local helpers ----------------

/// One tool's human-readable block: green `name:` header, one line per
/// version, `(none)` when there is nothing installed
fn print_local_section(name: &str, versions: &[String], current: Option<&str>) {
    println!("{}", ogreen(format!("{name}:")));
    if versions.is_empty() {
        println!("  (none)");
        return;
    }
    for version in versions {
        let markers = markers_for(version, current, None, None);
        println!("{}", render_version_line(version, &markers));
    }
}

/// Next-step hint for a tool with nothing installed; the suggestion depends
/// on whether its plugin is available
fn hint_not_installed(name: &str) {
    if plugin_path(name).exists() {
        print_hint(
            &format!("no local version of '{name}' is installed"),
            &[format!("uvman list {name} --remote"), format!("uvman install {name}@lts")],
        );
    } else {
        print_hint(
            &format!("plugin '{name}' is not installed"),
            &[format!("uvman plugin install {name}")],
        );
    }
}

/// All installed tools with their versions (newest first), sorted by name.
/// A missing tools/ dir counts as "nothing installed" — read-only commands
/// must not fail on an uninitialized layout.
fn collect_installed_tools() -> Result<Vec<(String, Vec<String>)>, UError> {
    let entries = match std::fs::read_dir(tools_dir()) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(UError::IoError { source }),
    };
    let mut tools = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && let Some(name) = entry.file_name().to_str()
        {
            tools.push((name.to_string(), installed_desc(name)));
        }
    }
    tools.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(tools)
}

/// Installed versions of one tool, newest first. The directory scan is
/// reused from toolset; re-sorted here so local and remote share one
/// "newest first" definition.
fn installed_desc(name: &str) -> Vec<String> {
    let mut versions = toolset::installed_versions(name);
    versions.sort_by_key(|v| std::cmp::Reverse(desc_key(v)));
    versions
}

// ---------------- Remote helpers ----------------

// ---------------- Pager (long --remote listings) ----------------

/// One row of the pager: a version plus its annotations
struct VersionRow {
    version: String,
    markers: Vec<Marker>,
}

/// Scroll/search state of one pager session
#[derive(Default)]
struct PagerState {
    /// First visible row
    offset: usize,
    /// Search input being typed (Some = input mode)
    query: Option<String>,
    /// Confirmed search term (for `n`, next match)
    needle: Option<String>,
    /// Row index of the last jumped-to match
    last_match: Option<usize>,
    /// Transient footer message (e.g. "not found: x"); cleared on next key
    message: Option<String>,
}

/// Marker/search styles used by the pager
const CURRENT_STYLE: Style = Style::new().fg(Color::Green);
const DIM_STYLE: Style = Style::new().add_modifier(Modifier::DIM);
const SEARCH_MATCH_STYLE: Style = Style::new().add_modifier(Modifier::REVERSED);

/// Where the user left the pager, for echoing the visible page after exit
struct PagerExit {
    /// First visible row at quit time
    offset: usize,
    /// Visible row count at quit time
    viewport: usize,
    /// q/Esc quit: echo the page; Ctrl-C: plain interrupt, no echo
    echo: bool,
}

/// Interactive scroll view (alternate screen, ratatui) for listings longer
/// than the pager threshold. Falls back to a plain full print when the
/// terminal can't be set up.
///
/// Keys: ↑/↓/j/k line scroll, ←/→ page, Home/End/g/G jump, `/` search
/// (Enter jumps to the match, `n` next match). q/Esc quit and echo the page
/// on screen for the next command; Ctrl-C quits without echoing.
fn show_pager(tool: &str, rows: &[VersionRow]) -> io::Result<()> {
    // try_init sets up raw mode + the alternate screen (and a panic hook that
    // restores the terminal first); restore undoes both
    let mut terminal = ratatui::try_init()?;
    terminal.hide_cursor()?;
    let exit = pager_loop(&mut terminal, tool, rows);
    let _ = terminal.show_cursor();
    ratatui::restore();
    let exit = exit?;
    if exit.echo {
        print_page(tool, rows, exit.offset, exit.viewport);
    }
    Ok(())
}

fn pager_loop(
    terminal: &mut DefaultTerminal, tool: &str, rows: &[VersionRow],
) -> io::Result<PagerExit> {
    // Version strings are the search corpus
    let versions: Vec<&str> = rows.iter().map(|r| r.version.as_str()).collect();
    let mut state = PagerState::default();

    loop {
        // Fixed header/footer rows; the versions scroll in between
        let viewport = terminal.size()?.height.saturating_sub(2).max(1) as usize;
        state.offset = clamp_offset(state.offset, rows.len(), viewport);

        terminal.draw(|frame| render_pager(frame, tool, rows, &state, viewport))?;

        // Offsets mutated below are clamped again in the next frame
        let Event::Key(key) = event::read()? else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        // Ctrl-C quits from any mode (as an interrupt: no page echo)
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(PagerExit { offset: state.offset, viewport, echo: false });
        }
        state.message = None;

        match state.query.take() {
            // ---- search input mode ----
            Some(mut q) => match key.code {
                KeyCode::Enter => {
                    if q.is_empty() {
                        continue;
                    }
                    // First match after the viewport top, wrapping to the top
                    match find_match(&versions, &q, state.offset + 1) {
                        Some(i) => {
                            state.needle = Some(q);
                            state.last_match = Some(i);
                            // Jump only when the match isn't already on screen
                            if i < state.offset || i >= state.offset + viewport {
                                state.offset = i;
                            }
                        },
                        None => state.message = Some(format!("not found: {q}")),
                    }
                },
                KeyCode::Esc => {}, // cancel; query stays dropped
                KeyCode::Backspace => {
                    q.pop();
                    state.query = Some(q);
                },
                KeyCode::Char(c) => {
                    q.push(c);
                    state.query = Some(q);
                },
                _ => state.query = Some(q), // other keys don't disturb the input
            },
            // ---- normal mode ----
            None => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    return Ok(PagerExit { offset: state.offset, viewport, echo: true });
                },
                KeyCode::Char('/') => state.query = Some(String::new()),
                KeyCode::Char('n') => match state.needle.clone() {
                    Some(term) => {
                        // Next match after the last one, wrapping to the top
                        let origin = state.last_match.map_or(0, |m| m + 1);
                        match find_match(&versions, &term, origin) {
                            Some(i) => {
                                state.last_match = Some(i);
                                if i < state.offset || i >= state.offset + viewport {
                                    state.offset = i;
                                }
                            },
                            None => state.message = Some(format!("not found: {term}")),
                        }
                    },
                    None => state.message = Some("no search yet — press /".into()),
                },
                KeyCode::Down | KeyCode::Char('j') => state.offset += 1,
                KeyCode::Up | KeyCode::Char('k') => state.offset = state.offset.saturating_sub(1),
                KeyCode::Right => state.offset += viewport,
                KeyCode::Left => state.offset = state.offset.saturating_sub(viewport),
                KeyCode::Home | KeyCode::Char('g') => state.offset = 0,
                KeyCode::End | KeyCode::Char('G') => state.offset = usize::MAX,
                _ => {},
            },
        }
    }
}

/// One frame: fixed `tool:` header, the visible version rows, and the
/// status/key footer
fn render_pager(
    frame: &mut Frame, tool: &str, rows: &[VersionRow], state: &PagerState, viewport: usize,
) {
    // Live highlight: the query being typed wins over the confirmed term
    let active = state.query.as_deref().filter(|q| !q.is_empty()).or(state.needle.as_deref());

    let footer = match (&state.query, &state.message) {
        (Some(q), _) => dim_line(format!("/{q}▏ · Enter jump · Esc cancel")),
        (None, Some(msg)) => dim_line(msg.clone()),
        (None, None) => {
            let total = rows.len();
            let (start, end) =
                (state.offset + 1, state.offset + viewport.min(total - state.offset));
            dim_line(format!(
                "{start}-{end}/{total} versions · / search · n next · ←→ page · q quit (echo page)"
            ))
        },
    };

    let mut list = Vec::with_capacity(viewport);
    for row in &rows[state.offset..state.offset + viewport] {
        let matched = match active {
            Some(term) if matches_prefix(&row.version, term) => {
                matched_prefix_len(term, &row.version)
            },
            _ => 0,
        };
        list.push(Line::from(version_spans(&row.version, &row.markers, matched)));
    }

    let [header_area, list_area, footer_area]: [Rect; 3] = Layout::vertical([
        Constraint::Length(1), // tool header
        Constraint::Min(1),    // scrolling version list
        Constraint::Length(1), // status/key footer
    ])
    .areas(frame.area());
    frame.render_widget(
        Paragraph::new(Line::styled(format!("{tool}:"), CURRENT_STYLE)),
        header_area,
    );
    frame.render_widget(Paragraph::new(list), list_area);
    frame.render_widget(Paragraph::new(footer), footer_area);
}

/// First row at/after `start` (wrapping to the top) whose version matches
/// `term` as a version prefix — 22 matches 22.x.y, never x.22.y or x.y.22 —
/// the same semantics as `use <tool>@<term>`. None when nothing matches.
fn find_match(versions: &[&str], term: &str, start: usize) -> Option<usize> {
    if versions.is_empty() || term.is_empty() {
        return None;
    }
    (0..versions.len())
        .map(|i| (i + start) % versions.len())
        .find(|&i| matches_prefix(versions[i], term))
}

/// Number of leading version chars covered by the prefix match (drives the
/// search highlight); both sides compared without their v/V prefix
fn matched_prefix_len(term: &str, version: &str) -> usize {
    bare_version(term).chars().count().min(bare_version(version).chars().count())
}

/// One version row as styled spans: ` - <version> (<markers>)`, with the
/// first `highlight_len` chars of the bare version in reverse video when
/// searching. The v/V prefix, if present, is left unhighlighted.
fn version_spans(version: &str, markers: &[Marker], highlight_len: usize) -> Vec<Span<'static>> {
    let mut spans = vec![Span::raw(" - ")];
    let bare = bare_version(version);
    let bare_start = version.len() - bare.len();
    if bare_start > 0 {
        spans.push(Span::raw(version[..bare_start].to_string()));
    }
    let hl = highlight_len.min(bare.chars().count());
    let (matched, tail) =
        bare.char_indices().nth(hl).map_or((bare, ""), |(byte, _)| (&bare[..byte], &bare[byte..]));
    if !matched.is_empty() {
        spans.push(Span::styled(matched.to_string(), SEARCH_MATCH_STYLE));
    }
    if !tail.is_empty() {
        spans.push(Span::raw(tail.to_string()));
    }
    if !markers.is_empty() {
        spans.push(Span::raw(" ("));
        for (i, marker) in markers.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(", "));
            }
            let style = match marker {
                Marker::Current => CURRENT_STYLE,
                _ => DIM_STYLE,
            };
            spans.push(Span::styled(marker.label(), style));
        }
        spans.push(Span::raw(")"));
    }
    spans
}

fn dim_line(text: String) -> Line<'static> {
    Line::styled(text, DIM_STYLE)
}

/// Non-interactive full print of a tool's rows (also the pager fallback)
fn print_plain(tool: &str, rows: &[VersionRow]) {
    println!("{}", ogreen(format!("{tool}:")));
    for row in rows {
        println!("{}", render_version_line(&row.version, &row.markers));
    }
}

/// Echo the page the user was viewing when they quit the pager, so the
/// versions stay visible (and copyable) for the next `install` / `use`
fn print_page(tool: &str, rows: &[VersionRow], offset: usize, viewport: usize) {
    println!("{}", ogreen(format!("{tool}:")));
    let end = offset + viewport.min(rows.len() - offset);
    for row in &rows[offset..end] {
        println!("{}", render_version_line(&row.version, &row.markers));
    }
    let (start, total) = (offset + 1, rows.len());
    let example = rows[offset].version.as_str();
    println!("{}", odim(format!("… {start}-{end}/{total} · e.g. uvman use {tool}@{example}")));
}

/// Clamp a scroll offset so the viewport stays inside the line list
fn clamp_offset(offset: usize, total: usize, viewport: usize) -> usize {
    if total <= viewport {
        return 0;
    }
    offset.min(total - viewport)
}

/// The `@lts` target: newest entry carrying LTS metadata, as (version,
/// codename). Entries are expected newest-first (see [`sort_remote_desc`]).
fn newest_lts(versions: &[RemoteVersion]) -> Option<(String, String)> {
    versions
        .iter()
        .find_map(|v| v.lts.as_ref().map(|codename| (v.version.clone(), codename.clone())))
}

// ---------------- Shared display helpers ----------------

/// Whether a remote version matches a prefix request (`22` / `22.1`), using
/// the same semantics as `use <tool>@<prefix>`: the request must continue
/// past the version's segments, so request `22` doesn't match version `2`,
/// while `2.0` matches a bare `2`. A leading `v`/`V` is ignored on both
/// sides, so a raw `v20.19.2` (plugins without version_pattern) matches `20`.
fn matches_prefix(version: &str, prefix: &str) -> bool {
    let (version, prefix) = (bare_version(version), bare_version(prefix));
    version == prefix
        || version.starts_with(&format!("{prefix}."))
        || prefix.starts_with(&format!("{version}."))
}

/// Version-line annotations, rendered after the version in parentheses
#[derive(Debug, PartialEq, Eq)]
enum Marker {
    /// Version recorded as active by `use`
    Current,
    /// Newest remote version (the `@latest` target)
    Latest,
    /// Newest LTS release, with its codename
    Lts(String),
}

impl Marker {
    fn label(&self) -> String {
        match self {
            Marker::Current => "current".into(),
            Marker::Latest => "latest".into(),
            Marker::Lts(codename) => format!("lts: {codename}"),
        }
    }
}

/// Markers applying to `version`, given the active/latest/lts references.
/// The latest/lts references are resolved on the full version set before
/// filtering, so a filtered view only annotates versions it actually shows.
/// All comparisons ignore a leading `v`/`V`, so a `20.19.2` current entry
/// still marks a raw `v20.19.2` remote line. Order: user state first, then
/// release-line info.
fn markers_for(
    version: &str, current: Option<&str>, latest: Option<&str>, lts: Option<(&str, &str)>,
) -> Vec<Marker> {
    let bare = bare_version(version);
    let same = |other: &str| bare_version(other) == bare;
    let mut markers = Vec::new();
    if current.is_some_and(same) {
        markers.push(Marker::Current);
    }
    if latest.is_some_and(same) {
        markers.push(Marker::Latest);
    }
    if let Some((lts_version, codename)) = lts
        && same(lts_version)
    {
        markers.push(Marker::Lts(codename.to_string()));
    }
    markers
}

/// One display line: ` - <version>` plus markers (current green, release-line
/// info dim)
fn render_version_line(version: &str, markers: &[Marker]) -> String {
    if markers.is_empty() {
        return format!(" - {version}");
    }
    let labels: Vec<String> = markers
        .iter()
        .map(|m| match m {
            Marker::Current => ogreen(m.label()).to_string(),
            _ => odim(m.label()).to_string(),
        })
        .collect();
    format!(" - {version} ({})", labels.join(", "))
}

/// Version string without its leading v/V; the canonical form for
/// comparisons (prefix filter, markers)
fn bare_version(s: &str) -> &str {
    s.trim_start_matches(['v', 'V'])
}

/// Parse a version string (v/V prefix tolerated); None on failure
fn parse_version(s: &str) -> Option<semver::Version> {
    semver::Version::parse(bare_version(s)).ok()
}

/// Sort remote entries newest-first: parsed semver (prerelease-aware) ranks
/// before unparsable aliases, ties broken by the raw string. Descending so
/// the newest release is the first line.
fn sort_remote_desc(versions: &mut [RemoteVersion]) {
    versions.sort_by_key(|v| std::cmp::Reverse(desc_key(&v.version)));
}

/// Descending sort key: (semver-parseable, parsed version, raw string)
fn desc_key(s: &str) -> (u8, Option<semver::Version>, String) {
    match parse_version(s) {
        Some(v) => (1, Some(v), s.to_string()),
        None => (0, None, s.to_string()),
    }
}

// ---------------- JSON output ----------------

/// JSON payload of one tool: the `versions` array (newest first) plus the
/// version `use` recorded as active (null when none)
#[derive(Serialize)]
struct LocalToolJson {
    tool: String,
    versions: Vec<String>,
    current: Option<String>,
}

/// Build the local-listing JSON from collected tools + the current table
fn local_json(tools: &[(String, Vec<String>)], currents: &CurrentTools) -> serde_json::Value {
    let value: Vec<LocalToolJson> = tools
        .iter()
        .map(|(name, versions)| LocalToolJson {
            tool: name.clone(),
            versions: versions.clone(),
            current: currents.tools.get(name).map(|e| e.version.clone()),
        })
        .collect();
    serde_json::json!(value)
}

fn print_json(value: &serde_json::Value) -> Result<(), UError> {
    let json =
        serde_json::to_string_pretty(value).map_err(|source| UError::JsonError { source })?;
    println!("{json}");
    Ok(())
}

fn current_of<'a>(currents: &'a CurrentTools, tool: &str) -> Option<&'a str> {
    currents.tools.get(tool).map(|e| e.version.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::current::{CurrentEntry, CurrentTools};

    fn rv(version: &str, lts: Option<&str>) -> RemoteVersion {
        RemoteVersion { version: version.into(), lts: lts.map(Into::into) }
    }

    #[test]
    fn test_sort_remote_desc_semver_first() {
        // Newest first; v-prefix parses; prerelease ranks below its release
        let mut v = vec![
            rv("9.0.1", None),
            rv("v8.0.0", None),
            rv("22.0.0-nightly20260101", None),
            rv("22.0.0", None),
            rv("10.0.0", None),
        ];
        sort_remote_desc(&mut v);
        let names: Vec<&str> = v.iter().map(|r| r.version.as_str()).collect();
        assert_eq!(names, vec!["22.0.0", "22.0.0-nightly20260101", "10.0.0", "9.0.1", "v8.0.0"]);
    }

    #[test]
    fn test_sort_remote_desc_unparsable_last() {
        // Aliases that don't parse (beta/alpha) rank after real versions;
        // among themselves they fall back to lexicographic order
        let mut v = vec![rv("beta", None), rv("1.0.0", None), rv("alpha", None)];
        sort_remote_desc(&mut v);
        let names: Vec<&str> = v.iter().map(|r| r.version.as_str()).collect();
        assert_eq!(names, vec!["1.0.0", "beta", "alpha"]);
    }

    #[test]
    fn test_latest_and_lts_after_desc_sort() {
        let mut v =
            vec![rv("26.7.0", None), rv("22.12.0", Some("Jod")), rv("20.11.0", Some("Iron"))];
        sort_remote_desc(&mut v);
        // Newest overall = first entry of the descending list (`@latest`)
        assert_eq!(v.first().map(|r| r.version.as_str()), Some("26.7.0"));
        let (version, codename) = newest_lts(&v).unwrap();
        assert_eq!((version.as_str(), codename.as_str()), ("22.12.0", "Jod"));
    }

    #[test]
    fn test_matches_prefix() {
        assert!(matches_prefix("22.0.0", "22"));
        assert!(matches_prefix("22.19.0", "22.19"));
        assert!(matches_prefix("22", "22"));
        // Request continues past the version: 2.0 matches a bare 2
        assert!(matches_prefix("2", "2.0"));
        // Request 22 must not match version 2
        assert!(!matches_prefix("2.0.0", "22"));
        assert!(!matches_prefix("22.0.0", "23"));
        // A leading v/V on either side is ignored (raw plugin data)
        assert!(matches_prefix("v20.19.2", "20"));
        assert!(matches_prefix("20.19.2", "v20"));
    }

    #[test]
    fn test_markers_for_order_and_content() {
        // User state first, then release-line info
        let m = markers_for("22.19.0", Some("22.19.0"), Some("22.19.0"), Some(("22.19.0", "Jod")));
        assert_eq!(m, vec![Marker::Current, Marker::Latest, Marker::Lts("Jod".into())]);

        // v/V prefix differences don't hide a marker (raw remote data)
        let m = markers_for("v20.19.2", Some("20.19.2"), None, None);
        assert_eq!(m, vec![Marker::Current]);

        // A plain version gets no markers
        let m = markers_for("20.11.0", Some("22.19.0"), Some("24.19.0"), Some(("22.19.0", "Jod")));
        assert!(m.is_empty());
    }

    #[test]
    fn test_marker_labels() {
        assert_eq!(Marker::Current.label(), "current");
        assert_eq!(Marker::Latest.label(), "latest");
        assert_eq!(Marker::Lts("Jod".into()).label(), "lts: Jod");
    }

    #[test]
    fn test_clamp_offset() {
        // Content shorter than the viewport stays at the top
        assert_eq!(clamp_offset(10, 5, 20), 0);
        assert_eq!(clamp_offset(0, 60, 20), 0);
        assert_eq!(clamp_offset(5, 60, 20), 5);
        // Deep offsets stop at the last full page (End key relies on this)
        assert_eq!(clamp_offset(usize::MAX, 60, 20), 40);
    }

    #[test]
    fn test_find_match_prefix_semantics() {
        let versions = ["26.8.1", "22.19.0", "22.0.0", "2.22.0", "v21.0.0"];
        // Searching 22 matches only the 22.x lines, never 2.22.0 or 26.8.1
        assert_eq!(find_match(&versions, "22", 0), Some(1));
        // `n` walks the remaining 22.x line, then wraps back to the first
        assert_eq!(find_match(&versions, "22", 2), Some(2));
        assert_eq!(find_match(&versions, "22", 3), Some(1));
        // The v/V prefix on either side is ignored (raw plugin data)
        assert_eq!(find_match(&versions, "21", 0), Some(4));
        assert_eq!(find_match(&versions, "v22", 0), Some(1));
        // No match anywhere / empty term / empty corpus
        assert_eq!(find_match(&versions, "18", 0), None);
        assert_eq!(find_match(&versions, "", 0), None);
        assert_eq!(find_match(&[], "x", 0), None);
    }

    #[test]
    fn test_matched_prefix_len() {
        // The highlighted part is the matched prefix of the bare version
        assert_eq!(matched_prefix_len("22", "22.19.0"), 2);
        assert_eq!(matched_prefix_len("22.19", "22.19.0"), 5);
        // Request continuing past the version highlights the whole version
        assert_eq!(matched_prefix_len("2.0", "2"), 1);
        // v/V prefixes are stripped on both sides
        assert_eq!(matched_prefix_len("v22", "v22.19.0"), 2);
    }

    #[test]
    fn test_version_spans_highlight_and_markers() {
        use ratatui::prelude::{Color, Modifier};

        let spans = version_spans("22.19.0", &[Marker::Current, Marker::Lts("Jod".into())], 2);
        let texts: Vec<&str> = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, vec![" - ", "22", ".19.0", " (", "current", ", ", "lts: Jod", ")"]);
        // The matched prefix carries reverse video, current is green, the
        // lts codename is dim
        assert!(spans[1].style.add_modifier.contains(Modifier::REVERSED));
        assert_eq!(spans[4].style.fg, Some(Color::Green));
        assert!(spans[6].style.add_modifier.contains(Modifier::DIM));

        // A v/V prefix stays unhighlighted; no markers means no suffix spans
        let spans = version_spans("v22.19.0", &[], 2);
        let texts: Vec<&str> = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, vec![" - ", "v", "22", ".19.0"]);
    }

    #[test]
    fn test_local_json_shape() {
        // The array field is `versions` (plural), newest first, with the
        // active version alongside; a tool without an active version is null
        let tools = vec![
            ("node".to_string(), vec!["22.19.0".to_string(), "20.19.2".to_string()]),
            ("go".to_string(), vec!["1.23.4".to_string()]),
        ];
        let mut currents = CurrentTools::default();
        currents.tools.insert("node".into(), CurrentEntry { version: "22.19.0".into() });

        let value = local_json(&tools, &currents);
        assert_eq!(value[0]["tool"], "node");
        assert_eq!(value[0]["versions"][0], "22.19.0");
        assert_eq!(value[0]["versions"][1], "20.19.2");
        assert_eq!(value[0]["current"], "22.19.0");
        assert_eq!(value[1]["tool"], "go");
        assert!(value[1]["current"].is_null());
    }
}
