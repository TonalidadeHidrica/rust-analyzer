//! The main loop of `rust-analyzer` responsible for dispatching LSP
//! requests/replies and notifications back to the client.
use std::{
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};

use always_assert::always;
use crossbeam_channel::{select, Receiver};
use ide_db::base_db::{SourceDatabaseExt, VfsPath};
use lsp_server::{Connection, Notification, Request};
use lsp_types::notification::Notification as _;
use vfs::{ChangeKind, FileId};

use crate::{
    config::Config,
    dispatch::{NotificationDispatcher, RequestDispatcher},
    from_proto,
    global_state::{file_id_to_url, url_to_file_id, GlobalState},
    handlers, lsp_ext,
    lsp_utils::{apply_document_changes, notification_is, Progress},
    mem_docs::DocumentData,
    reload::{self, BuildDataProgress, ProjectWorkspaceProgress},
    Result,
};

pub fn main_loop(config: Config, connection: Connection) -> Result<()> {
    tracing::info!("initial config: {:#?}", config);

    // Windows scheduler implements priority boosts: if thread waits for an
    // event (like a condvar), and event fires, priority of the thread is
    // temporary bumped. This optimization backfires in our case: each time the
    // `main_loop` schedules a task to run on a threadpool, the worker threads
    // gets a higher priority, and (on a machine with fewer cores) displaces the
    // main loop! We work-around this by marking the main loop as a
    // higher-priority thread.
    //
    // https://docs.microsoft.com/en-us/windows/win32/procthread/scheduling-priorities
    // https://docs.microsoft.com/en-us/windows/win32/procthread/priority-boosts
    // https://github.com/rust-lang/rust-analyzer/issues/2835
    #[cfg(windows)]
    unsafe {
        use winapi::um::processthreadsapi::*;
        let thread = GetCurrentThread();
        let thread_priority_above_normal = 1;
        SetThreadPriority(thread, thread_priority_above_normal);
    }

    GlobalState::new(connection.sender, config).run(connection.receiver)
}

enum Event {
    Lsp(lsp_server::Message),
    Task(Task),
    Vfs(vfs::loader::Message),
    Flycheck(flycheck::Message),
}

#[derive(Debug)]
pub(crate) enum Task {
    Response(lsp_server::Response),
    Retry(lsp_server::Request),
    Diagnostics(Vec<(FileId, Vec<lsp_types::Diagnostic>)>),
    PrimeCaches(PrimeCachesProgress),
    FetchWorkspace(ProjectWorkspaceProgress),
    FetchBuildData(BuildDataProgress),
}

#[derive(Debug)]
pub(crate) enum PrimeCachesProgress {
    Begin,
    Report(ide::ParallelPrimeCachesProgress),
    End { cancelled: bool },
}

impl fmt::Debug for Event {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let debug_verbose_not = |not: &Notification, f: &mut fmt::Formatter| {
            f.debug_struct("Notification").field("method", &not.method).finish()
        };

        match self {
            Event::Lsp(lsp_server::Message::Notification(not)) => {
                if notification_is::<lsp_types::notification::DidOpenTextDocument>(not)
                    || notification_is::<lsp_types::notification::DidChangeTextDocument>(not)
                {
                    return debug_verbose_not(not, f);
                }
            }
            Event::Task(Task::Response(resp)) => {
                return f
                    .debug_struct("Response")
                    .field("id", &resp.id)
                    .field("error", &resp.error)
                    .finish();
            }
            _ => (),
        }
        match self {
            Event::Lsp(it) => fmt::Debug::fmt(it, f),
            Event::Task(it) => fmt::Debug::fmt(it, f),
            Event::Vfs(it) => fmt::Debug::fmt(it, f),
            Event::Flycheck(it) => fmt::Debug::fmt(it, f),
        }
    }
}

impl GlobalState {
    fn run(mut self, inbox: Receiver<lsp_server::Message>) -> Result<()> {
        if self.config.linked_projects().is_empty()
            && self.config.detached_files().is_empty()
            && self.config.notifications().cargo_toml_not_found
        {
            self.show_and_log_error("rust-analyzer failed to discover workspace".to_string(), None);
        };

        if self.config.did_save_text_document_dynamic_registration() {
            let save_registration_options = lsp_types::TextDocumentSaveRegistrationOptions {
                include_text: Some(false),
                text_document_registration_options: lsp_types::TextDocumentRegistrationOptions {
                    document_selector: Some(vec![
                        lsp_types::DocumentFilter {
                            language: None,
                            scheme: None,
                            pattern: Some("**/*.rs".into()),
                        },
                        lsp_types::DocumentFilter {
                            language: None,
                            scheme: None,
                            pattern: Some("**/Cargo.toml".into()),
                        },
                        lsp_types::DocumentFilter {
                            language: None,
                            scheme: None,
                            pattern: Some("**/Cargo.lock".into()),
                        },
                    ]),
                },
            };

            let registration = lsp_types::Registration {
                id: "textDocument/didSave".to_string(),
                method: "textDocument/didSave".to_string(),
                register_options: Some(serde_json::to_value(save_registration_options).unwrap()),
            };
            self.send_request::<lsp_types::request::RegisterCapability>(
                lsp_types::RegistrationParams { registrations: vec![registration] },
                |_, _| (),
            );
        }

        self.fetch_workspaces_queue.request_op("startup".to_string());
        if let Some(cause) = self.fetch_workspaces_queue.should_start_op() {
            self.fetch_workspaces(cause);
        }

        while let Some(event) = self.next_event(&inbox) {
            if let Event::Lsp(lsp_server::Message::Notification(not)) = &event {
                if not.method == lsp_types::notification::Exit::METHOD {
                    return Ok(());
                }
            }
            self.handle_event(event)?
        }

        Err("client exited without proper shutdown sequence".into())
    }

    fn next_event(&self, inbox: &Receiver<lsp_server::Message>) -> Option<Event> {
        select! {
            recv(inbox) -> msg =>
                msg.ok().map(Event::Lsp),

            recv(self.task_pool.receiver) -> task =>
                Some(Event::Task(task.unwrap())),

            recv(self.loader.receiver) -> task =>
                Some(Event::Vfs(task.unwrap())),

            recv(self.flycheck_receiver) -> task =>
                Some(Event::Flycheck(task.unwrap())),
        }
    }

    fn handle_event(&mut self, event: Event) -> Result<()> {
        let loop_start = Instant::now();
        // NOTE: don't count blocking select! call as a loop-turn time
        let _p = profile::span("GlobalState::handle_event");

        tracing::debug!("handle_event({:?})", event);
        let task_queue_len = self.task_pool.handle.len();
        if task_queue_len > 0 {
            tracing::info!("task queue len: {}", task_queue_len);
        }

        let was_quiescent = self.is_quiescent();
        match event {
            Event::Lsp(msg) => match msg {
                lsp_server::Message::Request(req) => self.on_new_request(loop_start, req),
                lsp_server::Message::Notification(not) => {
                    self.on_notification(not)?;
                }
                lsp_server::Message::Response(resp) => self.complete_request(resp),
            },
            Event::Task(mut task) => {
                let _p = profile::span("GlobalState::handle_event/task");
                let mut prime_caches_progress = Vec::new();
                loop {
                    match task {
                        Task::Response(response) => self.respond(response),
                        Task::Retry(req) => self.on_request(req),
                        Task::Diagnostics(diagnostics_per_file) => {
                            for (file_id, diagnostics) in diagnostics_per_file {
                                self.diagnostics.set_native_diagnostics(file_id, diagnostics)
                            }
                        }
                        Task::PrimeCaches(progress) => match progress {
                            PrimeCachesProgress::Begin => prime_caches_progress.push(progress),
                            PrimeCachesProgress::Report(_) => {
                                match prime_caches_progress.last_mut() {
                                    Some(last @ PrimeCachesProgress::Report(_)) => {
                                        // Coalesce subsequent update events.
                                        *last = progress;
                                    }
                                    _ => prime_caches_progress.push(progress),
                                }
                            }
                            PrimeCachesProgress::End { .. } => prime_caches_progress.push(progress),
                        },
                        Task::FetchWorkspace(progress) => {
                            let (state, msg) = match progress {
                                ProjectWorkspaceProgress::Begin => (Progress::Begin, None),
                                ProjectWorkspaceProgress::Report(msg) => {
                                    (Progress::Report, Some(msg))
                                }
                                ProjectWorkspaceProgress::End(workspaces) => {
                                    self.fetch_workspaces_queue.op_completed(workspaces);

                                    let old = Arc::clone(&self.workspaces);
                                    self.switch_workspaces("fetched workspace".to_string());
                                    let workspaces_updated = !Arc::ptr_eq(&old, &self.workspaces);

                                    if self.config.run_build_scripts() && workspaces_updated {
                                        self.fetch_build_data_queue
                                            .request_op(format!("workspace updated"));
                                    }

                                    (Progress::End, None)
                                }
                            };

                            self.report_progress("Fetching", state, msg, None);
                        }
                        Task::FetchBuildData(progress) => {
                            let (state, msg) = match progress {
                                BuildDataProgress::Begin => (Some(Progress::Begin), None),
                                BuildDataProgress::Report(msg) => {
                                    (Some(Progress::Report), Some(msg))
                                }
                                BuildDataProgress::End(build_data_result) => {
                                    self.fetch_build_data_queue.op_completed(build_data_result);

                                    self.switch_workspaces("fetched build data".to_string());

                                    (Some(Progress::End), None)
                                }
                            };

                            if let Some(state) = state {
                                self.report_progress("Loading", state, msg, None);
                            }
                        }
                    }

                    // Coalesce multiple task events into one loop turn
                    task = match self.task_pool.receiver.try_recv() {
                        Ok(task) => task,
                        Err(_) => break,
                    };
                }

                for progress in prime_caches_progress {
                    let (state, message, fraction);
                    match progress {
                        PrimeCachesProgress::Begin => {
                            state = Progress::Begin;
                            message = None;
                            fraction = 0.0;
                        }
                        PrimeCachesProgress::Report(report) => {
                            state = Progress::Report;

                            message = match &report.crates_currently_indexing[..] {
                                [crate_name] => Some(format!(
                                    "{}/{} ({})",
                                    report.crates_done, report.crates_total, crate_name
                                )),
                                [crate_name, rest @ ..] => Some(format!(
                                    "{}/{} ({} + {} more)",
                                    report.crates_done,
                                    report.crates_total,
                                    crate_name,
                                    rest.len()
                                )),
                                _ => None,
                            };

                            fraction = Progress::fraction(report.crates_done, report.crates_total);
                        }
                        PrimeCachesProgress::End { cancelled } => {
                            state = Progress::End;
                            message = None;
                            fraction = 1.0;

                            self.prime_caches_queue.op_completed(());
                            if cancelled {
                                self.prime_caches_queue
                                    .request_op("restart after cancellation".to_string());
                            }
                        }
                    };

                    self.report_progress("Indexing", state, message, Some(fraction));
                }
            }
            Event::Vfs(mut task) => {
                let _p = profile::span("GlobalState::handle_event/vfs");
                loop {
                    match task {
                        vfs::loader::Message::Loaded { files } => {
                            let vfs = &mut self.vfs.write().0;
                            for (path, contents) in files {
                                let path = VfsPath::from(path);
                                if !self.mem_docs.contains(&path) {
                                    vfs.set_file_contents(path, contents);
                                }
                            }
                        }
                        vfs::loader::Message::Progress { n_total, n_done, config_version } => {
                            always!(config_version <= self.vfs_config_version);

                            self.vfs_progress_config_version = config_version;
                            self.vfs_progress_n_total = n_total;
                            self.vfs_progress_n_done = n_done;

                            let state = if n_done == 0 {
                                Progress::Begin
                            } else if n_done < n_total {
                                Progress::Report
                            } else {
                                assert_eq!(n_done, n_total);
                                Progress::End
                            };
                            self.report_progress(
                                "Roots Scanned",
                                state,
                                Some(format!("{}/{}", n_done, n_total)),
                                Some(Progress::fraction(n_done, n_total)),
                            )
                        }
                    }
                    // Coalesce many VFS event into a single loop turn
                    task = match self.loader.receiver.try_recv() {
                        Ok(task) => task,
                        Err(_) => break,
                    }
                }
            }
            Event::Flycheck(mut task) => {
                let _p = profile::span("GlobalState::handle_event/flycheck");
                loop {
                    match task {
                        flycheck::Message::AddDiagnostic { workspace_root, diagnostic } => {
                            let snap = self.snapshot();
                            let diagnostics =
                                crate::diagnostics::to_proto::map_rust_diagnostic_to_lsp(
                                    &self.config.diagnostics_map(),
                                    &diagnostic,
                                    &workspace_root,
                                    &snap,
                                );
                            for diag in diagnostics {
                                match url_to_file_id(&self.vfs.read().0, &diag.url) {
                                    Ok(file_id) => self.diagnostics.add_check_diagnostic(
                                        file_id,
                                        diag.diagnostic,
                                        diag.fix,
                                    ),
                                    Err(err) => {
                                        tracing::error!(
                                            "File with cargo diagnostic not found in VFS: {}",
                                            err
                                        );
                                    }
                                };
                            }
                        }

                        flycheck::Message::Progress { id, progress } => {
                            let (state, message) = match progress {
                                flycheck::Progress::DidStart => {
                                    self.diagnostics.clear_check();
                                    (Progress::Begin, None)
                                }
                                flycheck::Progress::DidCheckCrate(target) => {
                                    (Progress::Report, Some(target))
                                }
                                flycheck::Progress::DidCancel => (Progress::End, None),
                                flycheck::Progress::DidFinish(result) => {
                                    if let Err(err) = result {
                                        self.show_and_log_error(
                                            "cargo check failed".to_string(),
                                            Some(err.to_string()),
                                        );
                                    }
                                    (Progress::End, None)
                                }
                            };

                            // When we're running multiple flychecks, we have to include a disambiguator in
                            // the title, or the editor complains. Note that this is a user-facing string.
                            let title = if self.flycheck.len() == 1 {
                                match self.config.flycheck() {
                                    Some(config) => format!("{}", config),
                                    None => "cargo check".to_string(),
                                }
                            } else {
                                format!("cargo check (#{})", id + 1)
                            };
                            self.report_progress(&title, state, message, None);
                        }
                    }
                    // Coalesce many flycheck updates into a single loop turn
                    task = match self.flycheck_receiver.try_recv() {
                        Ok(task) => task,
                        Err(_) => break,
                    }
                }
            }
        }

        let state_changed = self.process_changes();
        let memdocs_added_or_removed = self.mem_docs.take_changes();

        if self.is_quiescent() {
            if !was_quiescent {
                for flycheck in &self.flycheck {
                    flycheck.update();
                }
                if self.config.prefill_caches() {
                    self.prime_caches_queue.request_op("became quiescent".to_string());
                }
            }

            if !was_quiescent || state_changed {
                // Refresh semantic tokens if the client supports it.
                if self.config.semantic_tokens_refresh() {
                    self.semantic_tokens_cache.lock().clear();
                    self.send_request::<lsp_types::request::SemanticTokensRefresh>((), |_, _| ());
                }

                // Refresh code lens if the client supports it.
                if self.config.code_lens_refresh() {
                    self.send_request::<lsp_types::request::CodeLensRefresh>((), |_, _| ());
                }
            }

            if !was_quiescent || state_changed || memdocs_added_or_removed {
                if self.config.publish_diagnostics() {
                    self.update_diagnostics()
                }
            }
        }

        if let Some(diagnostic_changes) = self.diagnostics.take_changes() {
            for file_id in diagnostic_changes {
                let db = self.analysis_host.raw_database();
                let source_root = db.file_source_root(file_id);
                if db.source_root(source_root).is_library {
                    // Only publish diagnostics for files in the workspace, not from crates.io deps
                    // or the sysroot.
                    // While theoretically these should never have errors, we have quite a few false
                    // positives particularly in the stdlib, and those diagnostics would stay around
                    // forever if we emitted them here.
                    continue;
                }

                let url = file_id_to_url(&self.vfs.read().0, file_id);
                let diagnostics = self.diagnostics.diagnostics_for(file_id).cloned().collect();
                let version = from_proto::vfs_path(&url)
                    .map(|path| self.mem_docs.get(&path).map(|it| it.version))
                    .unwrap_or_default();

                self.send_notification::<lsp_types::notification::PublishDiagnostics>(
                    lsp_types::PublishDiagnosticsParams { uri: url, diagnostics, version },
                );
            }
        }

        if self.config.cargo_autoreload() {
            if let Some(cause) = self.fetch_workspaces_queue.should_start_op() {
                self.fetch_workspaces(cause);
            }
        }

        if !self.fetch_workspaces_queue.op_in_progress() {
            if let Some(cause) = self.fetch_build_data_queue.should_start_op() {
                self.fetch_build_data(cause);
            }
        }

        if let Some(cause) = self.prime_caches_queue.should_start_op() {
            tracing::debug!(%cause, "will prime caches");
            let num_worker_threads = self.config.prime_caches_num_threads();

            self.task_pool.handle.spawn_with_sender({
                let analysis = self.snapshot().analysis;
                move |sender| {
                    sender.send(Task::PrimeCaches(PrimeCachesProgress::Begin)).unwrap();
                    let res = analysis.parallel_prime_caches(num_worker_threads, |progress| {
                        let report = PrimeCachesProgress::Report(progress);
                        sender.send(Task::PrimeCaches(report)).unwrap();
                    });
                    sender
                        .send(Task::PrimeCaches(PrimeCachesProgress::End {
                            cancelled: res.is_err(),
                        }))
                        .unwrap();
                }
            });
        }

        let status = self.current_status();
        if self.last_reported_status.as_ref() != Some(&status) {
            self.last_reported_status = Some(status.clone());

            if let (lsp_ext::Health::Error, Some(message)) = (status.health, &status.message) {
                self.show_message(lsp_types::MessageType::ERROR, message.clone());
            }

            if self.config.server_status_notification() {
                self.send_notification::<lsp_ext::ServerStatusNotification>(status);
            }
        }

        let loop_duration = loop_start.elapsed();
        if loop_duration > Duration::from_millis(100) && was_quiescent {
            tracing::warn!("overly long loop turn: {:?}", loop_duration);
            self.poke_rust_analyzer_developer(format!(
                "overly long loop turn: {:?}",
                loop_duration
            ));
        }
        Ok(())
    }

    fn on_new_request(&mut self, request_received: Instant, req: Request) {
        self.register_request(&req, request_received);
        self.on_request(req);
    }

    fn on_request(&mut self, req: Request) {
        if self.shutdown_requested {
            self.respond(lsp_server::Response::new_err(
                req.id,
                lsp_server::ErrorCode::InvalidRequest as i32,
                "Shutdown already requested.".to_owned(),
            ));
            return;
        }

        // Avoid flashing a bunch of unresolved references during initial load.
        if self.workspaces.is_empty() && !self.is_quiescent() {
            self.respond(lsp_server::Response::new_err(
                req.id,
                lsp_server::ErrorCode::ContentModified as i32,
                "waiting for cargo metadata or cargo check".to_owned(),
            ));
            return;
        }

        RequestDispatcher { req: Some(req), global_state: self }
            .on_sync_mut::<lsp_types::request::Shutdown>(|s, ()| {
                s.shutdown_requested = true;
                Ok(())
            })
            .on_sync_mut::<lsp_ext::ReloadWorkspace>(handlers::handle_workspace_reload)
            .on_sync_mut::<lsp_ext::MemoryUsage>(handlers::handle_memory_usage)
            .on_sync_mut::<lsp_ext::ShuffleCrateGraph>(handlers::handle_shuffle_crate_graph)
            .on_sync::<lsp_ext::JoinLines>(handlers::handle_join_lines)
            .on_sync::<lsp_ext::OnEnter>(handlers::handle_on_enter)
            .on_sync::<lsp_types::request::SelectionRangeRequest>(handlers::handle_selection_range)
            .on_sync::<lsp_ext::MatchingBrace>(handlers::handle_matching_brace)
            .on::<lsp_ext::AnalyzerStatus>(handlers::handle_analyzer_status)
            .on::<lsp_ext::SyntaxTree>(handlers::handle_syntax_tree)
            .on::<lsp_ext::ViewHir>(handlers::handle_view_hir)
            .on::<lsp_ext::ViewFileText>(handlers::handle_view_file_text)
            .on::<lsp_ext::ViewCrateGraph>(handlers::handle_view_crate_graph)
            .on::<lsp_ext::ViewItemTree>(handlers::handle_view_item_tree)
            .on::<lsp_ext::ExpandMacro>(handlers::handle_expand_macro)
            .on::<lsp_ext::ParentModule>(handlers::handle_parent_module)
            .on::<lsp_ext::Runnables>(handlers::handle_runnables)
            .on::<lsp_ext::RelatedTests>(handlers::handle_related_tests)
            .on::<lsp_ext::CodeActionRequest>(handlers::handle_code_action)
            .on::<lsp_ext::CodeActionResolveRequest>(handlers::handle_code_action_resolve)
            .on::<lsp_ext::HoverRequest>(handlers::handle_hover)
            .on::<lsp_ext::ExternalDocs>(handlers::handle_open_docs)
            .on::<lsp_ext::OpenCargoToml>(handlers::handle_open_cargo_toml)
            .on::<lsp_ext::MoveItem>(handlers::handle_move_item)
            .on::<lsp_ext::WorkspaceSymbol>(handlers::handle_workspace_symbol)
            .on::<lsp_ext::OnTypeFormatting>(handlers::handle_on_type_formatting)
            .on::<lsp_types::request::DocumentSymbolRequest>(handlers::handle_document_symbol)
            .on::<lsp_types::request::GotoDefinition>(handlers::handle_goto_definition)
            .on::<lsp_types::request::GotoDeclaration>(handlers::handle_goto_declaration)
            .on::<lsp_types::request::GotoImplementation>(handlers::handle_goto_implementation)
            .on::<lsp_types::request::GotoTypeDefinition>(handlers::handle_goto_type_definition)
            .on::<lsp_types::request::InlayHintRequest>(handlers::handle_inlay_hints)
            .on::<lsp_types::request::InlayHintResolveRequest>(handlers::handle_inlay_hints_resolve)
            .on::<lsp_types::request::Completion>(handlers::handle_completion)
            .on::<lsp_types::request::ResolveCompletionItem>(handlers::handle_completion_resolve)
            .on::<lsp_types::request::CodeLensRequest>(handlers::handle_code_lens)
            .on::<lsp_types::request::CodeLensResolve>(handlers::handle_code_lens_resolve)
            .on::<lsp_types::request::FoldingRangeRequest>(handlers::handle_folding_range)
            .on::<lsp_types::request::SignatureHelpRequest>(handlers::handle_signature_help)
            .on::<lsp_types::request::PrepareRenameRequest>(handlers::handle_prepare_rename)
            .on::<lsp_types::request::Rename>(handlers::handle_rename)
            .on::<lsp_types::request::References>(handlers::handle_references)
            .on::<lsp_types::request::Formatting>(handlers::handle_formatting)
            .on::<lsp_types::request::RangeFormatting>(handlers::handle_range_formatting)
            .on::<lsp_types::request::DocumentHighlightRequest>(handlers::handle_document_highlight)
            .on::<lsp_types::request::CallHierarchyPrepare>(handlers::handle_call_hierarchy_prepare)
            .on::<lsp_types::request::CallHierarchyIncomingCalls>(
                handlers::handle_call_hierarchy_incoming,
            )
            .on::<lsp_types::request::CallHierarchyOutgoingCalls>(
                handlers::handle_call_hierarchy_outgoing,
            )
            .on::<lsp_types::request::SemanticTokensFullRequest>(
                handlers::handle_semantic_tokens_full,
            )
            .on::<lsp_types::request::SemanticTokensFullDeltaRequest>(
                handlers::handle_semantic_tokens_full_delta,
            )
            .on::<lsp_types::request::SemanticTokensRangeRequest>(
                handlers::handle_semantic_tokens_range,
            )
            .on::<lsp_types::request::WillRenameFiles>(handlers::handle_will_rename_files)
            .on::<lsp_ext::Ssr>(handlers::handle_ssr)
            .finish();
    }

    fn on_notification(&mut self, not: Notification) -> Result<()> {
        NotificationDispatcher { not: Some(not), global_state: self }
            .on::<lsp_types::notification::Cancel>(|this, params| {
                let id: lsp_server::RequestId = match params.id {
                    lsp_types::NumberOrString::Number(id) => id.into(),
                    lsp_types::NumberOrString::String(id) => id.into(),
                };
                this.cancel(id);
                Ok(())
            })?
            .on::<lsp_types::notification::WorkDoneProgressCancel>(|_this, _params| {
                // Just ignore this. It is OK to continue sending progress
                // notifications for this token, as the client can't know when
                // we accepted notification.
                Ok(())
            })?
            .on::<lsp_types::notification::DidOpenTextDocument>(|this, params| {
                if let Ok(path) = from_proto::vfs_path(&params.text_document.uri) {
                    let already_exists = this
                        .mem_docs
                        .insert(path.clone(), DocumentData::new(params.text_document.version))
                        .is_err();
                    if already_exists {
                        tracing::error!("duplicate DidOpenTextDocument: {}", path)
                    }
                    this.vfs
                        .write()
                        .0
                        .set_file_contents(path, Some(params.text_document.text.into_bytes()));
                }
                Ok(())
            })?
            .on::<lsp_types::notification::DidChangeTextDocument>(|this, params| {
                if let Ok(path) = from_proto::vfs_path(&params.text_document.uri) {
                    match this.mem_docs.get_mut(&path) {
                        Some(doc) => {
                            // The version passed in DidChangeTextDocument is the version after all edits are applied
                            // so we should apply it before the vfs is notified.
                            doc.version = params.text_document.version;
                        }
                        None => {
                            tracing::error!("unexpected DidChangeTextDocument: {}", path);
                            return Ok(());
                        }
                    };

                    let vfs = &mut this.vfs.write().0;
                    let file_id = vfs.file_id(&path).unwrap();
                    let mut text = String::from_utf8(vfs.file_contents(file_id).to_vec()).unwrap();
                    apply_document_changes(&mut text, params.content_changes);

                    vfs.set_file_contents(path, Some(text.into_bytes()));
                }
                Ok(())
            })?
            .on::<lsp_types::notification::DidCloseTextDocument>(|this, params| {
                if let Ok(path) = from_proto::vfs_path(&params.text_document.uri) {
                    if this.mem_docs.remove(&path).is_err() {
                        tracing::error!("orphan DidCloseTextDocument: {}", path);
                    }

                    this.semantic_tokens_cache.lock().remove(&params.text_document.uri);

                    if let Some(path) = path.as_path() {
                        this.loader.handle.invalidate(path.to_path_buf());
                    }
                }
                Ok(())
            })?
            .on::<lsp_types::notification::DidSaveTextDocument>(|this, params| {
                for flycheck in &this.flycheck {
                    flycheck.update();
                }
                if let Ok(abs_path) = from_proto::abs_path(&params.text_document.uri) {
                    if reload::should_refresh_for_change(&abs_path, ChangeKind::Modify) {
                        this.fetch_workspaces_queue
                            .request_op(format!("DidSaveTextDocument {}", abs_path.display()));
                    }
                }
                Ok(())
            })?
            .on::<lsp_types::notification::DidChangeConfiguration>(|this, _params| {
                // As stated in https://github.com/microsoft/language-server-protocol/issues/676,
                // this notification's parameters should be ignored and the actual config queried separately.
                this.send_request::<lsp_types::request::WorkspaceConfiguration>(
                    lsp_types::ConfigurationParams {
                        items: vec![lsp_types::ConfigurationItem {
                            scope_uri: None,
                            section: Some("rust-analyzer".to_string()),
                        }],
                    },
                    |this, resp| {
                        tracing::debug!("config update response: '{:?}", resp);
                        let lsp_server::Response { error, result, .. } = resp;

                        match (error, result) {
                            (Some(err), _) => {
                                tracing::error!("failed to fetch the server settings: {:?}", err)
                            }
                            (None, Some(mut configs)) => {
                                if let Some(json) = configs.get_mut(0) {
                                    // Note that json can be null according to the spec if the client can't
                                    // provide a configuration. This is handled in Config::update below.
                                    let mut config = Config::clone(&*this.config);
                                    if let Err(error) = config.update(json.take()) {
                                        this.show_message(
                                            lsp_types::MessageType::WARNING,
                                            error.to_string(),
                                        );
                                    }
                                    this.update_configuration(config);
                                }
                            }
                            (None, None) => tracing::error!(
                                "received empty server settings response from the client"
                            ),
                        }
                    },
                );

                Ok(())
            })?
            .on::<lsp_types::notification::DidChangeWatchedFiles>(|this, params| {
                for change in params.changes {
                    if let Ok(path) = from_proto::abs_path(&change.uri) {
                        this.loader.handle.invalidate(path);
                    }
                }
                Ok(())
            })?
            .finish();
        Ok(())
    }

    fn update_diagnostics(&mut self) {
        let subscriptions = self
            .mem_docs
            .iter()
            .map(|path| self.vfs.read().0.file_id(path).unwrap())
            .collect::<Vec<_>>();

        tracing::trace!("updating notifications for {:?}", subscriptions);

        let snapshot = self.snapshot();
        self.task_pool.handle.spawn(move || {
            let diagnostics = subscriptions
                .into_iter()
                .filter_map(|file_id| {
                    handlers::publish_diagnostics(&snapshot, file_id)
                        .ok()
                        .map(|diags| (file_id, diags))
                })
                .collect::<Vec<_>>();
            Task::Diagnostics(diagnostics)
        })
    }
}
