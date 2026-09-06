//! Native DirectWrite regression probe. Run only through test-windows-settings.ps1,
//! which places these fixture windows on an isolated, inactive Windows desktop.
#[cfg(all(windows, feature = "desktop"))]
fn main() {
    use agentdictate_core::{Settings, TranscriptionProvider, WorkflowPhase, WorkflowSnapshot};
    use agentdictate_ui::{
        AgentDictateAssets, AgentDictateWindowFrame, Route, SettingsShell, ShellViewModel,
    };
    use gpui::{Application, Bounds, WindowBounds, WindowOptions, prelude::*, px, size};
    use gpui_component::Root;
    use std::sync::Arc;

    assert_eq!(
        std::env::args().nth(1).as_deref(),
        Some("--isolated-desktop")
    );
    Application::new().with_assets(AgentDictateAssets).run(|cx| {
        gpui_component::init(cx);
        for provider in [TranscriptionProvider::ChatGptSubscription, TranscriptionProvider::OpenAiApi] {
            for populated in [false, true] {
                for width in [720., 1180.] {
                    let mut settings = Settings { transcription_provider: provider, ..Default::default() };
                    if populated {
                        settings.vocabulary = agentdictate_core::parse_vocabulary("Example = exam pull\nCaf\u{e9}\n\u{6771}\u{4eac}\nRocket \u{1f680}").unwrap();
                        settings.project_context = "First line\n\nCaf\u{e9}, \u{6771}\u{4eac}, \u{1f680}\n".repeat(20);
                        settings.transcription_prompt = settings.project_context.clone();
                    }
                    let model = ShellViewModel::from_snapshot(Route::Settings, WorkflowSnapshot { phase: WorkflowPhase::Ready });
                    let bounds = Bounds::centered(None, size(px(width), px(1600.)), cx);
                    let window = cx.open_window(WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        focus: false,
                        ..Default::default()
                    }, move |window, cx| {
                        let shell = cx.new(|cx| SettingsShell::connected(model, settings, false, Arc::new(|_| panic!("fixture must not send commands")), window, cx));
                        let frame = cx.new(|_| AgentDictateWindowFrame::new(shell));
                        cx.new(|cx| Root::new(frame, window, cx))
                    }).expect("native settings fixture opens");
                    cx.update_window(window.into(), |_, window, cx| {
                        window.draw(cx).clear();
                        window.draw(cx).clear();
                        window.remove_window();
                    }).expect("native settings fixture renders");
                    println!("PASS native Settings: {provider:?}, populated={populated}, width={width}");
                }
            }
        }
        cx.quit();
    });
}

#[cfg(not(all(windows, feature = "desktop")))]
fn main() {}
