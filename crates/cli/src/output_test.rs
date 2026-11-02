//! The golden-test helper docs/OUTPUT.md asks of every command it converts: what the command
//! printed, as a terminal shows it — colour off — at default or at `-v`.

use std::future::Future;

/// Run `run`, capturing what it prints ([`arena_core::out::capture`]) at `verbose`, its prompts
/// answered from `answers` as typed (none left: refused, as with no terminal), and render what
/// a terminal would show — both streams in order, its error last as `main` prints it.
pub(crate) async fn rendered(verbose: bool, answers: &[&str], run: impl Future<Output = anyhow::Result<()>>) -> String {
    let cap = arena_core::out::capture(verbose);
    for a in answers {
        cap.answer(a);
    }
    let result = run.await;
    let mut text = cap.text();
    if let Err(e) = result {
        text.push_str(&format!("Error: {e:?}\n"));
    }
    text
}
