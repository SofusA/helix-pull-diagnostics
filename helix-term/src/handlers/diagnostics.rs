use std::collections::{HashMap, HashSet};
use std::time::Duration;

use helix_core::diagnostic::DiagnosticProvider;
use helix_core::syntax::LanguageServerFeature;
use helix_core::Uri;
use helix_event::{register_hook, send_blocking};
use helix_lsp::lsp;
use helix_view::document::Mode;
use helix_view::events::{DiagnosticsDidChange, DocumentDidChange, DocumentDidOpen};
use helix_view::handlers::diagnostics::DiagnosticEvent;
use helix_view::handlers::lsp::PullDiagnosticsEvent;
use helix_view::handlers::Handlers;
use helix_view::{DocumentId, Editor};
use tokio::time::Instant;

use crate::events::OnModeSwitch;
use crate::job;

pub(super) fn register_hooks(handlers: &Handlers) {
    register_hook!(move |event: &mut DiagnosticsDidChange<'_>| {
        if event.editor.mode != Mode::Insert {
            for (view, _) in event.editor.tree.views_mut() {
                send_blocking(&view.diagnostics_handler.events, DiagnosticEvent::Refresh)
            }
        }
        Ok(())
    });
    register_hook!(move |event: &mut OnModeSwitch<'_, '_>| {
        for (view, _) in event.cx.editor.tree.views_mut() {
            view.diagnostics_handler.active = event.new_mode != Mode::Insert;
        }
        Ok(())
    });

    let tx = handlers.pull_diagnostics.clone();
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        if event
            .doc
            .has_language_server_with_feature(LanguageServerFeature::PullDiagnostics)
        {
            let document_id = event.doc.id();
            send_blocking(&tx, PullDiagnosticsEvent { document_id });
        }
        Ok(())
    });

    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        if event
            .doc
            .has_language_server_with_feature(LanguageServerFeature::PullDiagnostics)
        {
            let document_id = event.doc.id();
            job::dispatch_blocking(move |editor, _| {
                let Some(doc) = editor.document(document_id) else {
                    return;
                };

                let language_servers =
                    doc.language_servers_with_feature(LanguageServerFeature::PullDiagnostics);

                for language_server in language_servers {
                    pull_diagnostics_for_document(doc, language_server);
                }
            })
        }

        Ok(())
    });
}

#[derive(Debug)]
pub(super) struct PullDiagnosticsHandler {
    document_ids: HashSet<DocumentId>,
}

impl PullDiagnosticsHandler {
    pub fn new() -> PullDiagnosticsHandler {
        PullDiagnosticsHandler {
            document_ids: [].into(),
        }
    }
}

impl helix_event::AsyncHook for PullDiagnosticsHandler {
    type Event = PullDiagnosticsEvent;

    fn handle_event(
        &mut self,
        event: Self::Event,
        existing_debounce: Option<tokio::time::Instant>,
    ) -> Option<tokio::time::Instant> {
        if existing_debounce.is_none() {
            dispatch_pull_diagnostic_for_document(event.document_id);
        }

        self.document_ids.insert(event.document_id);
        Some(Instant::now() + Duration::from_millis(500))
    }

    fn finish_debounce(&mut self) {
        for document_id in self.document_ids.clone() {
            dispatch_pull_diagnostic_for_document(document_id);
        }
    }
}

fn dispatch_pull_diagnostic_for_document(document_id: DocumentId) {
    job::dispatch_blocking(move |editor, _| {
        let Some(doc) = editor.document(document_id) else {
            return;
        };

        let language_servers =
            doc.language_servers_with_feature(LanguageServerFeature::PullDiagnostics);

        for language_server in language_servers {
            pull_diagnostics_for_document(doc, language_server);
        }
    })
}

pub fn pull_diagnostics_for_document(
    doc: &helix_view::Document,
    language_server: &helix_lsp::Client,
) {
    let Some(future) = language_server
        .text_document_diagnostic(doc.identifier(), doc.previous_diagnostic_id.clone())
    else {
        return;
    };

    let Some(uri) = doc.uri() else {
        return;
    };

    let provider = DiagnosticProvider::PullDiagnosticProvider(language_server.id());
    let document_id = doc.id();

    tokio::spawn(async move {
        match future.await {
            Ok(res) => {
                job::dispatch(move |editor, _| {
                    let response = match serde_json::from_value(res) {
                        Ok(result) => result,
                        Err(_) => return,
                    };

                    handle_pull_diagnostics_response(editor, response, provider, uri, document_id)
                })
                .await
            }
            Err(err) => log::error!("Pull diagnostic request failed: {err}"),
        }
    });
}

fn handle_pull_diagnostics_response(
    editor: &mut Editor,
    response: lsp::DocumentDiagnosticReport,
    provider: DiagnosticProvider,
    uri: Uri,
    document_id: DocumentId,
) {
    let Some(doc) = editor.document_mut(document_id) else {
        return;
    };

    match response {
        lsp::DocumentDiagnosticReport::Full(report) => {
            // Diagnostic for requested file
            editor.handle_lsp_diagnostics(
                provider,
                uri,
                None,
                report.full_document_diagnostic_report.items,
                report.full_document_diagnostic_report.result_id,
            );

            // Diagnostic for related files
            handle_document_diagnostic_report_kind(
                editor,
                document_id,
                report.related_documents,
                provider,
            );
        }
        lsp::DocumentDiagnosticReport::Unchanged(report) => {
            doc.previous_diagnostic_id =
                Some(report.unchanged_document_diagnostic_report.result_id);

            handle_document_diagnostic_report_kind(
                editor,
                document_id,
                report.related_documents,
                provider,
            );
        }
    }
}

fn handle_document_diagnostic_report_kind(
    editor: &mut Editor,
    document_id: DocumentId,
    report: Option<HashMap<lsp::Url, lsp::DocumentDiagnosticReportKind>>,
    provider: DiagnosticProvider,
) {
    for (url, report) in report.into_iter().flatten() {
        match report {
            lsp::DocumentDiagnosticReportKind::Full(report) => {
                let Ok(uri) = Uri::try_from(url) else {
                    return;
                };

                editor.handle_lsp_diagnostics(provider, uri, None, report.items, report.result_id);
            }
            lsp::DocumentDiagnosticReportKind::Unchanged(report) => {
                let Some(doc) = editor.document_mut(document_id) else {
                    return;
                };
                doc.previous_diagnostic_id = Some(report.result_id);
            }
        }
    }
}
