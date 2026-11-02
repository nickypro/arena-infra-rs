//! `arena update`'s changelog at default and at `-v` (docs/OUTPUT.md): the action-needed and
//! config lines in full, then each entry's heading alone; `-v` every entry in full, as before.

use arena_core::update::{describe_changes, new_since};

use super::{brief_changes, say_changes};
use crate::output_test::rendered;

const RELEASE: &str = "\
# Changelog

## 2026-11-03 · arena 2.13: quieter checks
- **`config check` says what's wrong.** The rest is `-v`'s.
### Action needed
- Re-run `arena cron install` to pick up the new line.

## 2026-11-02 · arena 2.12: shorter output by default
- **`pods up` says much less.**
### Config
- `ENDPOINT_TIMEOUT_MINS` (new, default 5).

## 2026-11-01 · arena 2.11: already installed
- old
";

const INSTALLED: &str = "# Changelog\n\n## 2026-11-01 · arena 2.11: already installed\n- old\n";

#[tokio::test]
async fn update_says_the_changelog_in_brief_and_in_full_with_v() {
    let changes = describe_changes(&new_since(INSTALLED, RELEASE));
    assert_eq!(
        brief_changes(&changes),
        [
            "Action needed:",
            "  ⚠ Re-run `arena cron install` to pick up the new line.",
            "Config changes:",
            "  • `ENDPOINT_TIMEOUT_MINS` (new, default 5).",
            "  · 2026-11-03 · arena 2.13: quieter checks",
            "  · 2026-11-02 · arena 2.12: shorter output by default",
        ]
    );
    let quiet = rendered(false, &[], async {
        say_changes(&changes);
        Ok(())
    })
    .await;
    assert_eq!(quiet, brief_changes(&changes).iter().map(|l| format!("{l}\n")).collect::<String>());
    let loud = rendered(true, &[], async {
        say_changes(&changes);
        Ok(())
    })
    .await;
    assert_eq!(loud, changes, "-v: the changelog as before");
}
