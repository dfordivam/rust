mod handlers;
mod subscriptions;

use std::{
    path::PathBuf,
    collections::{HashMap},
};

use threadpool::ThreadPool;
use serde::{Serialize, de::DeserializeOwned};
use crossbeam_channel::{bounded, Sender, Receiver};
use languageserver_types::{NumberOrString};
use libanalysis::{FileId, JobHandle, JobToken};
use gen_lsp_server::{RawRequest, RawNotification, RawMessage, RawResponse, ErrorCode};

use {
    req,
    Result,
    vfs::{self, FileEvent},
    server_world::{ServerWorldState, ServerWorld},
    main_loop::subscriptions::{Subscriptions},
};

enum Task {
    Respond(RawResponse),
    Notify(RawNotification),
}

pub(super) fn main_loop(
    root: PathBuf,
    msg_receriver: &mut Receiver<RawMessage>,
    msg_sender: &mut Sender<RawMessage>,
) -> Result<()> {
    let pool = ThreadPool::new(4);
    let (task_sender, task_receiver) = bounded::<Task>(16);
    let (fs_events_receiver, watcher) = vfs::watch(vec![root]);

    info!("server initialized, serving requests");
    let mut state = ServerWorldState::new();

    let mut pending_requests = HashMap::new();
    let mut subs = Subscriptions::new();
    main_loop_inner(
        &pool,
        msg_receriver,
        msg_sender,
        task_receiver.clone(),
        task_sender,
        fs_events_receiver,
        &mut state,
        &mut pending_requests,
        &mut subs,
    )?;

    info!("waiting for background jobs to finish...");
    task_receiver.for_each(|task| on_task(task, msg_sender, &mut pending_requests));
    pool.join();
    info!("...background jobs have finished");

    info!("waiting for file watcher to finish...");
    watcher.stop()?;
    info!("...file watcher has finished");
    Ok(())
}

fn main_loop_inner(
    pool: &ThreadPool,
    msg_receiver: &mut Receiver<RawMessage>,
    msg_sender: &mut Sender<RawMessage>,
    task_receiver: Receiver<Task>,
    task_sender: Sender<Task>,
    fs_receiver: Receiver<Vec<FileEvent>>,
    state: &mut ServerWorldState,
    pending_requests: &mut HashMap<u64, JobHandle>,
    subs: &mut Subscriptions,
) -> Result<u64> {
    let mut fs_receiver = Some(fs_receiver);
    loop {
        enum Event {
            Msg(RawMessage),
            Task(Task),
            Fs(Vec<FileEvent>),
            FsWatcherDead,
        }
        let event = select! {
            recv(msg_receiver, msg) => match msg {
                Some(msg) => Event::Msg(msg),
                None => bail!("client exited without shutdown"),
            },
            recv(task_receiver, task) => Event::Task(task.unwrap()),
            recv(fs_receiver, events) => match events {
                Some(events) => Event::Fs(events),
                None => Event::FsWatcherDead,
            }
        };
        let mut state_changed = false;
        match event {
            Event::FsWatcherDead => fs_receiver = None,
            Event::Task(task) => on_task(task, msg_sender, pending_requests),
            Event::Fs(events) => {
                trace!("fs change, {} events", events.len());
                state.apply_fs_changes(events);
                state_changed = true;
            }
            Event::Msg(msg) => {
                match msg {
                    RawMessage::Request(req) => {
                        let req = match req.cast::<req::Shutdown>() {
                            Ok((id, _params)) => return Ok(id),
                            Err(req) => req,
                        };
                        match on_request(state, pending_requests, pool, &task_sender, req)? {
                            None => (),
                            Some(req) => {
                                error!("unknown request: {:?}", req);
                                let resp = RawResponse::err(
                                    req.id,
                                    ErrorCode::MethodNotFound as i32,
                                    "unknown request".to_string(),
                                );
                                msg_sender.send(RawMessage::Response(resp))
                            }
                        }
                    }
                    RawMessage::Notification(not) => {
                        on_notification(msg_sender, state, pending_requests, subs, not)?;
                        state_changed = true;
                    }
                    RawMessage::Response(resp) => {
                        error!("unexpected response: {:?}", resp)
                    }
                }
            }
        };

        if state_changed {
            update_file_notifications_on_threadpool(
                pool,
                state.snapshot(),
                task_sender.clone(),
                subs.subscriptions(),
            )
        }
    }
}

fn on_task(
    task: Task,
    msg_sender: &mut Sender<RawMessage>,
    pending_requests: &mut HashMap<u64, JobHandle>,
) {
    match task {
        Task::Respond(response) => {
            if let Some(handle) = pending_requests.remove(&response.id) {
                assert!(handle.has_completed());
            }
            msg_sender.send(RawMessage::Response(response))
        }
        Task::Notify(n) =>
            msg_sender.send(RawMessage::Notification(n)),
    }
}

fn on_request(
    world: &mut ServerWorldState,
    pending_requests: &mut HashMap<u64, JobHandle>,
    pool: &ThreadPool,
    sender: &Sender<Task>,
    req: RawRequest,
) -> Result<Option<RawRequest>> {
    let mut pool_dispatcher = PoolDispatcher {
        req: Some(req),
        res: None,
        pool, world, sender
    };
    let req = pool_dispatcher
        .on::<req::SyntaxTree>(handlers::handle_syntax_tree)?
        .on::<req::ExtendSelection>(handlers::handle_extend_selection)?
        .on::<req::FindMatchingBrace>(handlers::handle_find_matching_brace)?
        .on::<req::JoinLines>(handlers::handle_join_lines)?
        .on::<req::OnTypeFormatting>(handlers::handle_on_type_formatting)?
        .on::<req::DocumentSymbolRequest>(handlers::handle_document_symbol)?
        .on::<req::WorkspaceSymbol>(handlers::handle_workspace_symbol)?
        .on::<req::GotoDefinition>(handlers::handle_goto_definition)?
        .on::<req::ParentModule>(handlers::handle_parent_module)?
        .on::<req::Runnables>(handlers::handle_runnables)?
        .on::<req::DecorationsRequest>(handlers::handle_decorations)?
        .on::<req::Completion>(handlers::handle_completion)?
        .on::<req::CodeActionRequest>(handlers::handle_code_action)?
        .finish();
    match req {
        Ok((id, handle)) => {
            let inserted = pending_requests.insert(id, handle).is_none();
            assert!(inserted, "duplicate request: {}", id);
            Ok(None)
        },
        Err(req) => Ok(Some(req)),
    }
}

fn on_notification(
    msg_sender: &mut Sender<RawMessage>,
    state: &mut ServerWorldState,
    pending_requests: &mut HashMap<u64, JobHandle>,
    subs: &mut Subscriptions,
    not: RawNotification,
) -> Result<()> {
    let not = match not.cast::<req::Cancel>() {
        Ok(params) => {
            let id = match params.id {
                NumberOrString::Number(id) => id,
                NumberOrString::String(id) => {
                    panic!("string id's not supported: {:?}", id);
                }
            };
            if let Some(handle) = pending_requests.remove(&id) {
                handle.cancel();
            }
            return Ok(())
        }
        Err(not) => not,
    };
    let not = match not.cast::<req::DidOpenTextDocument>() {
        Ok(params) => {
            let uri = params.text_document.uri;
            let path = uri.to_file_path()
                .map_err(|()| format_err!("invalid uri: {}", uri))?;
            let file_id = state.add_mem_file(path, params.text_document.text);
            subs.add_sub(file_id);
            return Ok(())
        }
        Err(not) => not,
    };
    let not = match not.cast::<req::DidChangeTextDocument>() {
        Ok(mut params) => {
            let uri = params.text_document.uri;
            let path = uri.to_file_path()
                .map_err(|()| format_err!("invalid uri: {}", uri))?;
            let text = params.content_changes.pop()
                .ok_or_else(|| format_err!("empty changes"))?
                .text;
            state.change_mem_file(path.as_path(), text)?;
            return Ok(())
        }
        Err(not) => not,
    };
    let not = match not.cast::<req::DidCloseTextDocument>() {
        Ok(params) => {
            let uri = params.text_document.uri;
            let path = uri.to_file_path()
                .map_err(|()| format_err!("invalid uri: {}", uri))?;
            let file_id = state.remove_mem_file(path.as_path())?;
            subs.remove_sub(file_id);
            let params = req::PublishDiagnosticsParams { uri, diagnostics: Vec::new() };
            let not = RawNotification::new::<req::PublishDiagnostics>(params);
            msg_sender.send(RawMessage::Notification(not));
            return Ok(())
        }
        Err(not) => not,
    };
    error!("unhandled notification: {:?}", not);
    Ok(())
}

struct PoolDispatcher<'a> {
    req: Option<RawRequest>,
    res: Option<(u64, JobHandle)>,
    pool: &'a ThreadPool,
    world: &'a ServerWorldState,
    sender: &'a Sender<Task>,
}

impl<'a> PoolDispatcher<'a> {
    fn on<'b, R>(
        &'b mut self,
        f: fn(ServerWorld, R::Params, JobToken) -> Result<R::Result>
    ) -> Result<&'b mut Self>
    where R: req::Request,
          R::Params: DeserializeOwned + Send + 'static,
          R::Result: Serialize + 'static,
    {
        let req = match self.req.take() {
            None => return Ok(self),
            Some(req) => req,
        };
        match req.cast::<R>() {
            Ok((id, params)) => {
                let (handle, token) = JobHandle::new();
                let world = self.world.snapshot();
                let sender = self.sender.clone();
                self.pool.execute(move || {
                    let resp = match f(world, params, token) {
                        Ok(resp) => RawResponse::ok(id, resp),
                        Err(e) => RawResponse::err(id, ErrorCode::InternalError as i32, e.to_string()),
                    };
                    let task = Task::Respond(resp);
                    sender.send(task);
                });
                self.res = Some((id, handle));
            }
            Err(req) => {
                self.req = Some(req)
            }
        }
        Ok(self)
    }

    fn finish(&mut self) -> ::std::result::Result<(u64, JobHandle), RawRequest> {
        match (self.res.take(), self.req.take()) {
            (Some(res), None) => Ok(res),
            (None, Some(req)) => Err(req),
            _ => unreachable!(),
        }
    }
}

fn update_file_notifications_on_threadpool(
    pool: &ThreadPool,
    world: ServerWorld,
    sender: Sender<Task>,
    subscriptions: Vec<FileId>,
) {
    pool.execute(move || {
        for file_id in subscriptions {
            match handlers::publish_diagnostics(world.clone(), file_id) {
                Err(e) => {
                    error!("failed to compute diagnostics: {:?}", e)
                }
                Ok(params) => {
                    let not = RawNotification::new::<req::PublishDiagnostics>(params);
                    sender.send(Task::Notify(not));
                }
            }
            match handlers::publish_decorations(world.clone(), file_id) {
                Err(e) => {
                    error!("failed to compute decorations: {:?}", e)
                }
                Ok(params) => {
                    let not = RawNotification::new::<req::PublishDecorations>(params);
                    sender.send(Task::Notify(not))
                }
            }
        }
    });
}
