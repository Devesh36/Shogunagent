//! Context-cache walk policy and in-memory cache (spec §3.10.2).
//!
//! The *policy* — BFS with depth ≤8, ≤300 elements, ≤32KB, role filtering, and total
//! skip of AXSecureTextField subtrees — is pure and tested here over an abstract [`AxNode`]
//! tree. The macOS layer implements [`AxNode`] via AXUIElement (with a 100ms messaging
//! timeout and a 250ms overall timebox expressed through `should_stop`), keeping every AX
//! call confined to that adapter (the "no collect-on-press" invariant, spec §3.10.3).

/// Accessibility roles relevant to text collection (spec §3.10.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    StaticText,
    TextArea,
    TextField,
    Heading,
    Link,
    Cell,
    /// A labelled interactive/leaf control such as a button, row, list item, or image.
    NamedControl,
    /// A named content container such as a web area or group. Webview apps often expose a whole
    /// message/card through this accessible name instead of nested static-text leaves.
    NamedContainer,
    /// Never read; the whole subtree is skipped (password fields). Adapters also return this for
    /// a text field whose labelling names a secret — see [`super::secret_field::is_secret_field`].
    SecureTextField,
    /// A browser/app chrome container — a toolbar or a tab strip. Its subtree is application
    /// furniture (address bar, bookmarks, tab titles), not the content the user is reading. It is
    /// traversed LAST, so a scarce element budget is spent on the page body before the chrome.
    /// Without this a browser walk drains all 300 elements on the toolbar and tab strip and never
    /// reaches the article/message — measured as `bytes=63` from a full Gmail thread.
    DeferredChrome,
    /// Any other role — traversed for children but not itself collected.
    Other,
}

impl Role {
    fn is_collectable(self) -> bool {
        matches!(
            self,
            Role::StaticText
                | Role::TextArea
                | Role::TextField
                | Role::Heading
                | Role::Link
                | Role::Cell
                | Role::NamedControl
                | Role::NamedContainer
        )
    }
}

/// Walk budgets (spec §3.10.2). Overridable for the Q3 retry loop (depth 8→6, 300→200).
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_depth: u32,
    pub max_elements: u32,
    pub max_bytes: usize,
    /// Issue #302 noise reduction, on for every walk: lines are cleaned by
    /// [`super::noise::clean_line`], a line already contained in an ancestor's collected text is
    /// not repeated, and inside deferred chrome (toolbars, tab strips) only editable text — the
    /// address bar, a search box — is read. Off only to measure the unreduced walk
    /// (`crates/ax-noise-bench`).
    pub reduce_noise: bool,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_depth: 8, max_elements: 300, max_bytes: 32 * 1024, reduce_noise: true }
    }
}

impl Limits {
    /// Walk for durable memory capture, for every app. Depth and element caps sit far above any
    /// real tree so the caller's time deadline (`should_stop`) is what bounds the walk, as is the
    /// text byte cap. Browser and Electron page content sits deep below the window chrome.
    pub const fn memory_capture() -> Self {
        Self { max_depth: 64, max_elements: 50_000, max_bytes: 32 * 1024, reduce_noise: true }
    }

    /// The same budgets with noise reduction off — the unreduced walk, kept for measurement.
    pub const fn without_noise_reduction(self) -> Self {
        Self {
            max_depth: self.max_depth,
            max_elements: self.max_elements,
            max_bytes: self.max_bytes,
            reduce_noise: false,
        }
    }
}

/// An ancestor's collected text longer than this is not used for the containment check: the check
/// is O(ancestor text) per line, and a huge container name is rare and not the webview card shape
/// the check exists for. Skipping it only means less dedup, never lost text.
const MAX_ANCESTOR_TEXT_BYTES: usize = 4 * 1024;

/// An abstract accessibility element. The macOS adapter implements this over AXUIElement;
/// tests implement it over a plain struct. `children` is only called within the depth
/// budget, so the adapter never over-fetches.
pub trait AxNode: Sized {
    fn role(&self) -> Role;
    /// Resolved text for the already-read role. Passing the role avoids another platform AX call.
    fn value_text(&self, role: Role) -> Option<String>;
    fn children(&self) -> Vec<Self>;
}

/// Outcome of one walk.
///
/// `text` contains each distinct normalized line once for the whole window. Identical text from
/// separate controls or table cells is intentionally collapsed to bound storage and indexing;
/// callers needing positional repetition must inspect the accessibility tree directly.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct WalkResult {
    pub text: String,
    pub text_bytes: usize,
    pub elements_visited: u32,
    pub depth_reached: u32,
    pub truncated: bool,
    /// True if `should_stop` fired (generation cancelled or 250ms timebox hit).
    pub partial: bool,
}

/// A run of sibling controls is a navigation bar / button row when at least this many of its
/// text-bearing members are short control labels and nothing else in the run carries text.
const MIN_BAR_MEMBERS: usize = 3;
/// "Short control label": one line of at most this many characters …
const MAX_CONTROL_LABEL_CHARS: usize = 32;
/// … and words.
const MAX_CONTROL_LABEL_WORDS: usize = 4;

/// A labelled control (button, row, list item) or link whose whole accessible text is one short
/// line — the shape of navigation, toolbar and action-button entries.
fn is_short_control(role: Role, lines: &[String]) -> bool {
    matches!(role, Role::NamedControl | Role::Link)
        && lines.len() == 1
        && lines[0].chars().count() <= MAX_CONTROL_LABEL_CHARS
        && lines[0].split_whitespace().count() <= MAX_CONTROL_LABEL_WORDS
}

/// One queued element: its depth, its nearest collected ancestor's text (noise reduction only) and
/// the sibling group it belongs to (its parent's visit number).
struct Queued<N> {
    node: N,
    depth: u32,
    ancestor_text: Option<std::rc::Rc<str>>,
    group: u64,
}

/// The sibling run being read. BFS pops a parent's children consecutively, so a run is complete
/// when the next popped element has another group.
#[derive(Default)]
struct SiblingRun {
    group: u64,
    /// Short control labels held back until the run shows whether it is a bar.
    pending: Vec<String>,
    controls: usize,
    /// A member carried other text: the run is content with some controls in it, not a bar.
    mixed: bool,
}

/// Output text under the byte budget, one distinct line at a time.
struct Collector {
    res: WalkResult,
    seen_lines: std::collections::HashSet<String>,
    max_bytes: usize,
}

impl Collector {
    /// Append one line; `false` once the byte budget is exhausted (the walk stops).
    fn push(&mut self, line: &str) -> bool {
        if !self.seen_lines.insert(line.to_string()) {
            return true;
        }
        let separator_bytes = usize::from(!self.res.text.is_empty());
        if self.res.text_bytes + separator_bytes + line.len() > self.max_bytes {
            self.res.truncated = true;
            return false;
        }
        if separator_bytes != 0 {
            self.res.text.push('\n');
            self.res.text_bytes += 1;
        }
        self.res.text.push_str(line);
        self.res.text_bytes += line.len();
        true
    }

    fn flush(&mut self, pending: &mut Vec<String>) -> bool {
        pending.drain(..).all(|line| self.push(&line))
    }

    /// Close a sibling run: a bar's held-back labels are dropped, anything else is written.
    fn finish_run(&mut self, run: &mut SiblingRun) -> bool {
        if !run.mixed && run.controls >= MIN_BAR_MEMBERS {
            run.pending.clear();
            return true;
        }
        self.flush(&mut run.pending)
    }
}

/// Walk `root` under `limits`, calling `should_stop` before each element so the adapter
/// can enforce cancellation and the time budget. BFS; SecureTextField subtrees are skipped.
///
/// With [`Limits::reduce_noise`] (issue #302) the walk also: cleans every line
/// ([`super::noise::clean_line`]); skips a line already inside its nearest collected ancestor's
/// text (the webview card shape — a container named with the whole message, then the same pieces
/// as leaves); reads only editable text inside deferred chrome; and drops a sibling run that is
/// nothing but [`MIN_BAR_MEMBERS`]+ short control labels (navigation bars, button rows, sidebars
/// of links). Every rule is about roles, tree shape and characters — never an app.
///
/// `N: Clone` because the macOS AXUIElement wrapper is a cheap CF retain; the queue owns
/// nodes so it can be built incrementally within the depth budget.
pub fn walk<N: AxNode + Clone>(root: &N, limits: Limits, mut should_stop: impl FnMut() -> bool) -> WalkResult {
    use std::rc::Rc;

    let reduce = limits.reduce_noise;
    let mut out = Collector {
        res: WalkResult::default(),
        seen_lines: std::collections::HashSet::new(),
        max_bytes: limits.max_bytes,
    };
    // Two tiers; depth 0 = root. `main` is the page body; `chrome` holds the subtrees rooted at a
    // DeferredChrome node (toolbars, tab strips) and is only touched once the main tier drains.
    // The element budget spans both, so total work is unchanged — but when it runs out it is the
    // chrome that goes unread, not the content. Order within a tier stays BFS.
    let mut main: std::collections::VecDeque<Queued<N>> = std::collections::VecDeque::new();
    let mut chrome: std::collections::VecDeque<Queued<N>> = std::collections::VecDeque::new();
    main.push_back(Queued { node: root.clone(), depth: 0, ancestor_text: None, group: 0 });
    let mut run = SiblingRun::default();
    let mut next_group: u64 = 1;

    'walk: loop {
        let (queued, in_chrome) = match main.pop_front() {
            Some(queued) => (queued, false),
            None => match chrome.pop_front() {
                Some(queued) => (queued, true),
                None => break,
            },
        };
        let Queued { node, depth, ancestor_text, group } = queued;
        if group != run.group {
            if !out.finish_run(&mut run) {
                break;
            }
            run = SiblingRun { group, ..SiblingRun::default() };
        }
        if should_stop() {
            out.res.partial = true;
            break;
        }
        if out.res.elements_visited >= limits.max_elements {
            out.res.truncated = true;
            break;
        }
        let role = node.role();
        if role == Role::SecureTextField {
            // Skip the node and its entire subtree — but the node was still visited, so it must
            // be charged against the element budget: a tree stuffed with secure fields must not
            // be able to iterate unboundedly (only the 250ms timebox would stop it otherwise).
            out.res.elements_visited += 1;
            continue;
        }
        out.res.elements_visited += 1;
        out.res.depth_reached = out.res.depth_reached.max(depth);

        // Inside chrome, labels (tab titles, bookmark and toolbar buttons) are furniture; only
        // editable text says what the user is doing there. Skipping them also skips their AX reads.
        let read_text = role.is_collectable()
            && (!reduce || !in_chrome || matches!(role, Role::TextField | Role::TextArea));
        let mut own_text = String::new();
        if read_text {
            if let Some(t) = node.value_text(role) {
                let lines: Vec<String> = if reduce {
                    t.lines()
                        .filter_map(super::noise::clean_line)
                        .map(std::borrow::Cow::into_owned)
                        .collect()
                } else {
                    t.lines().map(str::trim).filter(|line| !line.is_empty()).map(str::to_string).collect()
                };
                if reduce {
                    own_text = lines.join("\n");
                }
                let short_control = reduce && !run.mixed && is_short_control(role, &lines);
                let fresh = lines.into_iter().filter(|line| {
                    !ancestor_text.as_deref().is_some_and(|text| text.contains(line.as_str()))
                });
                if short_control {
                    run.controls += 1;
                    run.pending.extend(fresh);
                } else if !own_text.is_empty() || !reduce {
                    if reduce {
                        run.mixed = true;
                        if !out.flush(&mut run.pending) {
                            break 'walk;
                        }
                    }
                    for line in fresh {
                        if !out.push(&line) {
                            break 'walk;
                        }
                    }
                }
            }
        }

        if depth < limits.max_depth {
            // A chrome container's whole subtree is deferred; everything else stays on the main
            // tier. Once a branch is deferred it stays deferred, so the tab strip never jumps
            // ahead of the article just because it is shallower. With noise reduction a chrome
            // node's descendants stay on the chrome tier so they are still known to be chrome
            // when popped (without it they re-enter `main`, which is empty by then: same position,
            // but the chrome flag would be lost).
            let stays_chrome = reduce && in_chrome;
            let target = if stays_chrome || role == Role::DeferredChrome { &mut chrome } else { &mut main };
            let ancestor_text = if reduce && !own_text.is_empty() && own_text.len() <= MAX_ANCESTOR_TEXT_BYTES {
                Some(Rc::<str>::from(own_text))
            } else {
                ancestor_text
            };
            let group = next_group;
            next_group += 1;
            for child in node.children() {
                target.push_back(Queued { node: child, depth: depth + 1, ancestor_text: ancestor_text.clone(), group });
            }
        }
    }
    // A walk that ended mid-run (budget, cancellation, end of tree) writes what it held back
    // unless the run already qualifies as a bar.
    out.finish_run(&mut run);
    out.res
}

/// The in-memory current context (spec §3.10.2 step 6). Holds text for on-screen display;
/// this is the live cache, not a log — the privacy rule (no text in sinks) is enforced at
/// the harness/record boundary, not here.
#[derive(Clone, Debug, Default)]
pub struct ContextCache {
    pub gen: u64,
    pub bundle_id: String,
    pub pid: i32,
    pub window_title: String,
    pub text: String,
    pub text_bytes: usize,
    pub captured_at_ms: u64,
    pub duration_ms: u64,
    pub partial: bool,
    pub truncated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct Fake {
        role: Role,
        text: Option<&'static str>,
        children: Vec<Fake>,
    }
    impl Fake {
        fn leaf(role: Role, text: &'static str) -> Fake {
            Fake { role, text: Some(text), children: vec![] }
        }
        fn group(children: Vec<Fake>) -> Fake {
            Fake { role: Role::Other, text: None, children }
        }
    }
    impl AxNode for Fake {
        fn role(&self) -> Role {
            self.role
        }
        fn value_text(&self, _role: Role) -> Option<String> {
            self.text.map(|s| s.to_string())
        }
        fn children(&self) -> Vec<Self> {
            self.children.clone()
        }
    }

    fn never_stop() -> bool {
        false
    }

    #[test]
    fn collects_nested_text_in_order() {
        let tree = Fake::group(vec![
            Fake::leaf(Role::Heading, "Title"),
            Fake::group(vec![Fake::leaf(Role::StaticText, "body one"), Fake::leaf(Role::Link, "link")]),
        ]);
        let r = walk(&tree, Limits::default(), never_stop);
        assert_eq!(r.text, "Title\nbody one\nlink");
        assert_eq!(r.text_bytes, r.text.len());
        assert!(!r.truncated && !r.partial);
    }

    #[test]
    fn secure_field_subtree_is_skipped() {
        let tree = Fake::group(vec![
            Fake::leaf(Role::StaticText, "visible"),
            Fake {
                role: Role::SecureTextField,
                text: Some("hunter2"),
                children: vec![Fake::leaf(Role::StaticText, "child of secret")],
            },
        ]);
        let r = walk(&tree, Limits::default(), never_stop);
        assert!(r.text.contains("visible"));
        assert!(!r.text.contains("hunter2"));
        assert!(!r.text.contains("child of secret"));
    }

    #[test]
    fn depth_limit_stops_descent() {
        // Chain of groups 12 deep with a text leaf at the bottom.
        let mut node = Fake::leaf(Role::StaticText, "deep");
        for _ in 0..12 {
            node = Fake::group(vec![node]);
        }
        let limits = Limits { max_depth: 3, ..Limits::default() };
        let r = walk(&node, limits, never_stop);
        assert!(r.depth_reached <= 3);
        assert!(!r.text.contains("deep"));
    }

    #[test]
    fn memory_capture_reaches_browser_text_below_default_depth() {
        let mut node = Fake::leaf(Role::StaticText, "deep browser page text");
        for _ in 0..12 {
            node = Fake::group(vec![node]);
        }

        let shallow = walk(&node, Limits::default(), never_stop);
        let durable = walk(&node, Limits::memory_capture(), never_stop);

        assert!(shallow.text.is_empty());
        assert_eq!(durable.text, "deep browser page text");
    }

    #[test]
    fn memory_capture_reaches_renderer_text_far_below_the_window() {
        let mut node = Fake::leaf(Role::StaticText, "renderer page text");
        for _ in 0..40 {
            node = Fake::group(vec![node]);
        }

        assert_eq!(walk(&node, Limits::memory_capture(), never_stop).text, "renderer page text");
    }

    #[test]
    fn element_budget_truncates() {
        let kids: Vec<Fake> = (0..500).map(|_| Fake::leaf(Role::StaticText, "x")).collect();
        let tree = Fake::group(kids);
        let limits = Limits { max_elements: 50, ..Limits::default() };
        let r = walk(&tree, limits, never_stop);
        assert!(r.truncated);
        assert!(r.elements_visited <= 50);
    }

    #[test]
    fn byte_budget_truncates() {
        let kids: Vec<Fake> = (0..100)
            .map(|i| {
                let unique = format!("{i:03}{}", "x".repeat(97));
                Fake::leaf(Role::StaticText, Box::leak(unique.into_boxed_str()))
            })
            .collect();
        let tree = Fake::group(kids);
        let limits = Limits { max_bytes: 250, ..Limits::default() };
        let r = walk(&tree, limits, never_stop);
        assert!(r.truncated);
        assert!(r.text_bytes <= 250);
    }

    #[test]
    fn cancellation_marks_partial() {
        let tree = Fake::group((0..100).map(|_| Fake::leaf(Role::StaticText, "x")).collect());
        let mut n = 0;
        let r = walk(&tree, Limits::default(), || {
            n += 1;
            n > 5 // stop after a few elements
        });
        assert!(r.partial);
    }

    #[test]
    fn chrome_subtree_is_visited_after_content_under_a_tight_budget() {
        // The browser shape: a wide toolbar/tab strip full of collectable links sits before the
        // page body in document order. Under a scarce budget a plain BFS would spend it all on the
        // chrome and never reach the body — the bug this deferral fixes.
        let chrome_links: Vec<Fake> =
            (0..20).map(|_| Fake::leaf(Role::Link, "chrome-link")).collect();
        let tree = Fake::group(vec![
            Fake { role: Role::DeferredChrome, text: None, children: chrome_links },
            Fake::group(vec![Fake::leaf(Role::StaticText, "the article body")]),
        ]);
        // Budget: root + chrome container + body group + body text = exactly enough for content,
        // none left for the 20 chrome links in the deferred tier.
        let limits = Limits { max_elements: 4, ..Limits::default() };
        let r = walk(&tree, limits, never_stop);
        assert!(r.text.contains("the article body"), "content must be reached first: {:?}", r.text);
        assert!(!r.text.contains("chrome-link"), "chrome must be dropped, not the body: {:?}", r.text);
        assert!(r.truncated);
    }

    #[test]
    fn chrome_is_still_collected_when_the_budget_allows() {
        // Without noise reduction, deferral is about ORDER, not exclusion: with room, the chrome
        // text is still captured.
        let tree = Fake::group(vec![
            Fake { role: Role::DeferredChrome, text: None, children: vec![Fake::leaf(Role::Link, "tab title")] },
            Fake::leaf(Role::StaticText, "body"),
        ]);
        let r = walk(&tree, Limits::default().without_noise_reduction(), never_stop);
        assert!(r.text.contains("body"));
        assert!(r.text.contains("tab title"));
        // Body comes before the deferred chrome in the output.
        assert!(r.text.find("body") < r.text.find("tab title"), "content precedes chrome: {:?}", r.text);
    }

    #[test]
    fn non_collectable_roles_are_not_captured() {
        let tree = Fake::group(vec![
            Fake::leaf(Role::Other, "chrome"),
            Fake::leaf(Role::StaticText, "content"),
        ]);
        let r = walk(&tree, Limits::default(), never_stop);
        assert_eq!(r.text, "content");
    }

    #[test]
    fn repeated_accessible_names_are_stored_once() {
        let tree = Fake::group(vec![
            Fake::leaf(Role::NamedContainer, "same message"),
            Fake::leaf(Role::StaticText, "same message"),
            Fake::leaf(Role::NamedControl, "different control"),
        ]);

        let r = walk(&tree, Limits::default(), never_stop);

        assert_eq!(r.text, "same message\ndifferent control");
    }

    #[test]
    fn noise_reduction_reads_only_editable_text_inside_chrome() {
        let tree = Fake::group(vec![
            Fake {
                role: Role::DeferredChrome,
                text: None,
                children: vec![
                    Fake::leaf(Role::NamedControl, "Back"),
                    Fake::leaf(Role::TextField, "https://news.example.com/launch"),
                    Fake::group(vec![Fake::leaf(Role::NamedControl, "Other tab title")]),
                ],
            },
            Fake::leaf(Role::StaticText, "body"),
        ]);
        let r = walk(&tree, Limits::default(), never_stop);
        assert_eq!(r.text, "body\nhttps://news.example.com/launch");
    }

    #[test]
    fn noise_reduction_cleans_lines_and_drops_symbol_only_ones() {
        let tree = Fake::group(vec![
            Fake::leaf(Role::StaticText, "•"),
            Fake::leaf(Role::NamedControl, "\u{E5D2}"),
            Fake::leaf(Role::StaticText, "Quarterly   review\u{200B}"),
            Fake::leaf(Role::StaticText, "Quarterly review"),
            Fake::leaf(Role::Cell, "42"),
        ]);
        let r = walk(&tree, Limits::default(), never_stop);
        assert_eq!(r.text, "Quarterly review\n42");
        let raw = walk(&tree, Limits::default().without_noise_reduction(), never_stop);
        assert!(raw.text.contains('•'), "{:?}", raw.text);
    }

    #[test]
    fn a_leaf_already_inside_its_container_text_is_not_repeated() {
        // The webview card shape: the container's accessible name is the whole message; its leaves
        // repeat the pieces. The text stays searchable once.
        let card = Fake {
            role: Role::NamedContainer,
            text: Some("Alice Chen 10:42 Can we move the launch review to Thursday?"),
            children: vec![
                Fake::leaf(Role::StaticText, "Alice Chen"),
                Fake::leaf(Role::StaticText, "Can we move the launch review to Thursday?"),
                Fake::leaf(Role::NamedControl, "Add reaction"),
            ],
        };
        let r = walk(&Fake::group(vec![card.clone()]), Limits::default(), never_stop);
        assert_eq!(r.text, "Alice Chen 10:42 Can we move the launch review to Thursday?\nAdd reaction");
        let raw = walk(&Fake::group(vec![card]), Limits::default().without_noise_reduction(), never_stop);
        assert_eq!(raw.text.lines().count(), 4);
    }

    #[test]
    fn noise_reduction_still_skips_secret_fields() {
        let tree = Fake::group(vec![
            Fake::leaf(Role::StaticText, "Sign in"),
            Fake {
                role: Role::SecureTextField,
                text: Some("hunter2"),
                children: vec![Fake::leaf(Role::StaticText, "child of secret")],
            },
        ]);
        for limits in [Limits::default(), Limits::memory_capture()] {
            assert!(limits.reduce_noise);
            let r = walk(&tree, limits, never_stop);
            assert_eq!(r.text, "Sign in");
        }
    }
}
