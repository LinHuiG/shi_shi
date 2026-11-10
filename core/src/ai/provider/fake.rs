//! A scripted driver for offline tests: never starts a process or
//! touches the network.
use super::*;

/// One scripted outcome. `Ok` replies pass through the shared loop
/// only when tools are offered; otherwise they are returned directly.
#[derive(Clone)]
pub enum Step {
    Reply(Value, Value),
    Fail(String),
    /// Report an invalid output, then fail.
    Invalid(String),
    /// Wait until the request is cancelled or its deadline passes.
    Hang,
    /// Call the context tool `name` with `arguments` and an optional
    /// content budget, record the answer in `reads` and go on.
    Read(String, Value, Option<usize>),
}

#[derive(Default)]
pub struct Fake {
    pub steps: Mutex<std::collections::VecDeque<Step>>,
    /// `(kind, prompt)` of every request.
    pub seen: Mutex<Vec<(String, String)>>,
    /// Answers of [`Step::Read`]: the text, or `failure:<kind>`.
    pub reads: Mutex<Vec<String>>,
}

/// A job without context, session or key; events are dropped.
pub fn job(route: Route, config: Config, prepared: Prepared, root: PathBuf) -> Job {
    let deadline = Instant::now() + Duration::from_secs(30);
    let (config, cancel) = (Arc::new(config), Cancel::default());
    Job {
        kind: route.kind.clone(),
        route,
        config,
        prepared,
        session: None,
        root,
        trace: None,
        key: None,
        cancel,
        deadline,
        context: None,
        events: Box::new(|_| {}),
    }
}

impl Fake {
    pub fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self { steps: Mutex::new(steps.into()), ..Default::default() })
    }

    pub fn reply(value: Value) -> Arc<Self> {
        Self::new(vec![Step::Reply(
            value,
            json!({"input_tokens":10,"output_tokens":5,"cached_input_tokens":2,"total_tokens":15}),
        )])
    }

    pub fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

impl Driver for Fake {
    /// [`Step::Hang`] waits asynchronously on the request's own cancel
    /// flag and deadline, holding no thread; other steps run blocking.
    fn send(self: Arc<Self>, job: Job) -> Sending {
        if !matches!(self.steps.lock().unwrap().front(), Some(Step::Hang)) {
            return send_blocking(self, job);
        }
        self.steps.lock().unwrap().pop_front();
        Box::pin(async move {
            self.seen.lock().unwrap().push((job.kind.clone(), job.prepared.prompt.clone()));
            let cancelled = tokio::select! {
                _ = job.cancel.cancelled() => true,
                _ = tokio::time::sleep_until(job.deadline.into()) => false,
            };
            let reply = Err(if cancelled { super::super::CANCELLED.into() } else { "AI 总时限已到".into() });
            Done { reply, session: job.session }
        })
    }

    fn analyze(&self, job: &Job, mut context: Option<&mut dyn Context>, event: &mut dyn FnMut(Event)) -> Reply {
        self.seen.lock().unwrap().push((job.kind.clone(), job.prepared.prompt.clone()));
        event(Event::SessionStarted("fake-thread".into()));
        event(Event::Stream { kind: "phase".into(), text: "正在接收响应".into() });
        let step = loop {
            let step = self.steps.lock().unwrap().pop_front().unwrap_or(Step::Fail("假驱动没有更多脚本".into()));
            let Step::Read(name, arguments, budget) = step else { break step };
            let answer = match context.as_deref_mut() {
                None => "failure:Denied".to_string(),
                Some(context) => match context.read(&name, &arguments, budget) {
                    Ok(ToolResult::Text(text)) => text,
                    Ok(other) => other.messages("fake").map(|(m, _)| m.to_string()).unwrap_or_default(),
                    Err(f) => format!("failure:{:?}", f.kind()),
                },
            };
            self.reads.lock().unwrap().push(answer);
        };
        match step {
            Step::Read(..) => unreachable!(),
            Step::Reply(mut value, usage) => {
                event(Event::Stream { kind: "content".into(), text: value.to_string() });
                event(Event::Usage(usage.clone()));
                if let Some(trace) = &job.trace {
                    trace.put("output.txt", value.to_string());
                }
                // The shared loop's final check, as Tools runs it.
                if let Err(e) = crate::ai::schema::check(&job.kind, &value) {
                    event(Event::OutputValidationFailed);
                    return Err(format!("AI 输出校验失败：{e}"));
                }
                if let Some(context) = context.as_deref_mut() {
                    let had = value.get("_analysis_degraded").is_some();
                    if let Err(e) = context.validate_output(&job.kind, &mut value) {
                        event(Event::OutputValidationFailed);
                        return Err(format!("AI 输出校验失败：{e:?}"));
                    }
                    let degraded = !had && value.as_object_mut().and_then(|m| m.remove("_analysis_degraded")).is_some();
                    context.finish(&mut value);
                    if degraded {
                        value["_analysis_degraded"] = json!(true);
                    }
                }
                Ok((value, usage))
            }
            Step::Fail(error) => Err(error),
            Step::Invalid(error) => {
                event(Event::OutputValidationFailed);
                Err(error)
            }
            Step::Hang => unreachable!("Hang is answered by send"),
        }
    }
}
