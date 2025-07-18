use crate::*;
use anyhow::Result;
use serde::Serialize;
use stream_future::stream;
use trylog::macros::*;

/// The status when calling [`GameViewModel::open_game`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "t", content = "data")]
pub enum OpenGameStatus {
    /// Loading the settings.
    LoadSettings,
    /// Loading the global records.
    LoadGlobalRecords,
    /// Loading the records.
    LoadRecords,
    /// The game is loaded.
    Loaded,
}

/// A view model of Ayaka.
/// It manages all settings and provides high-level APIs.
pub struct GameViewModel<S: SettingsManager, M: RawModule + Send + Sync + 'static> {
    context: Option<Context<M>>,
    current_record: ActionRecord,
    current_raw_context: Option<RawContext>,
    settings_manager: S,
    settings: Option<Settings>,
    records: Vec<ActionRecord>,
    global_record: Option<GlobalRecord>,
}

impl<S: SettingsManager, M: RawModule + Send + Sync + 'static> GameViewModel<S, M> {
    /// Create a [`GameViewModel`] with a settings manager.
    pub fn new(settings_manager: S) -> Self {
        Self {
            settings_manager,
            context: None,
            current_record: ActionRecord::default(),
            current_raw_context: None,
            settings: None,
            records: vec![],
            global_record: None,
        }
    }

    /// Open the game with context.
    #[stream(OpenGameStatus, lifetime = 'a)]
    pub async fn open_game<'a>(&'a mut self, context: Context<M>) -> Result<()> {
        yield OpenGameStatus::LoadSettings;
        let settings = unwrap_or_default_log!(
            self.settings_manager.load_settings(),
            "Load settings failed"
        );
        self.settings = Some(settings);

        yield OpenGameStatus::LoadGlobalRecords;
        let global_record = unwrap_or_default_log!(
            self.settings_manager
                .load_global_record(&context.game().config.title),
            "Load global records failed"
        );
        self.global_record = Some(global_record);

        yield OpenGameStatus::LoadRecords;
        self.records = unwrap_or_default_log!(
            self.settings_manager
                .load_records(&context.game().config.title),
            "Load records failed"
        );
        self.context = Some(context);

        yield OpenGameStatus::Loaded;

        Ok(())
    }

    /// The [`Context`], should be called after [`Self::open_game`].
    pub fn context(&self) -> &Context<M> {
        self.context
            .as_ref()
            .expect("should be called after open_game")
    }

    /// The [`Context`], should be called after [`Self::open_game`].
    pub fn context_mut(&mut self) -> &mut Context<M> {
        self.context
            .as_mut()
            .expect("should be called after open_game")
    }

    /// The current [`ActionRecord`].
    pub fn record(&self) -> &ActionRecord {
        &self.current_record
    }

    /// The loaded [`Settings`].
    pub fn settings(&self) -> &Settings {
        self.settings
            .as_ref()
            .expect("should be called after open_game")
    }

    /// Set the [`Settings`].
    pub fn set_settings(&mut self, settings: Settings) {
        self.settings = Some(settings);
    }

    /// The loaded [`ActionRecord`]s.
    pub fn records(&self) -> &[ActionRecord] {
        &self.records
    }

    /// The loaded [`GlobalRecord`].
    pub fn global_record(&self) -> &GlobalRecord {
        self.global_record
            .as_ref()
            .expect("should be called after open_game")
    }

    /// The loaded [`GlobalRecord`].
    pub fn global_record_mut(&mut self) -> &mut GlobalRecord {
        self.global_record
            .as_mut()
            .expect("should be called after open_game")
    }

    /// Get the avaliable locales from paragraphs.
    pub fn avaliable_locale(&self) -> impl Iterator<Item = &Locale> {
        self.context().game().paras.keys()
    }

    /// Start a new game.
    /// The state of the model after this call is actually invalid.
    /// You should call `next_run` immediately after this call,
    /// to ensure there's content in the game, and switch to the
    /// first line of the first paragraph.
    pub fn init_new(&mut self) {
        let ctx = self.context().game().start_context();
        self.current_record = ActionRecord::default();
        // This is the start.
        self.current_raw_context = None;
        self.context_mut().set_context(ctx);
    }

    /// Start a game with record.
    pub fn init_context(&mut self, record: ActionRecord) {
        let mut ctx = record.last_ctx_with_game(self.context().game());
        self.current_record = record;
        // Update current raw context.
        self.current_raw_context = self.current_record.history.last().cloned();
        log::debug!("Context: {:?}", self.current_raw_context);
        // `ctx` points to the next raw context.
        ctx.cur_act += 1;
        self.context_mut().set_context(ctx);
    }

    /// Start a game with the index of records.
    pub fn init_context_by_index(&mut self, index: usize) {
        self.init_context(self.records()[index].clone())
    }

    fn push_history(&mut self, ctx: &RawContext) {
        let cur_text = self
            .context()
            .game()
            .find_para(
                &self.context().game().config.base_lang,
                &ctx.cur_base_para,
                &ctx.cur_para,
            )
            .and_then(|p| p.texts.get(ctx.cur_act));
        let is_text = cur_text
            .map(|line| matches!(line, Line::Text(_)))
            .unwrap_or_default();
        if is_text {
            self.current_record.history.push(ctx.clone());
        }
    }

    /// Step to the next run.
    pub fn next_run(&mut self) -> bool {
        let ctx = self.context_mut().next_run();
        if let Some(ctx) = &ctx {
            self.push_history(ctx);
            self.global_record_mut().update(ctx);
            log::debug!("{ctx:?}");
        }
        self.current_raw_context = ctx;
        self.current_raw_context.is_some()
    }

    /// Step back to the last run.
    pub fn next_back_run(&mut self) -> bool {
        if self.current_record.history.len() <= 1 {
            log::debug!("No action in the history.");
            false
        } else {
            // The last entry is the current one.
            // We don't assume that a user could call next_back_run when the
            // current run is empty.
            self.current_record.history.pop();
            // When we pop the current run, the last entry is what we want.
            self.current_raw_context = self.current_record.history.last().cloned();
            debug_assert!(self.current_raw_context.is_some());
            // We clone the (new) current run to set the "next" raw context.
            // We don't use the popped run to set the raw context,
            // because the empty runs are not recorded.
            let mut ctx = self
                .current_raw_context
                .clone()
                .expect("current raw context cannot be None");
            ctx.cur_act += 1;
            self.context_mut().set_context(ctx);
            true
        }
    }

    /// Get the current [`RawContext`].
    pub fn current_run(&self) -> Option<&RawContext> {
        self.current_raw_context.as_ref()
    }

    /// Get the current paragraph title.
    pub fn current_title(&self) -> Option<&String> {
        self.context()
            .current_paragraph_title(&self.settings().lang)
    }

    /// Get the current action by language.
    pub fn current_action(&self) -> Option<Action> {
        self.current_run().map(|raw_ctx| {
            unwrap_or_default_log!(
                self.context().get_action(&self.settings().lang, raw_ctx),
                "Cannot get action"
            )
        })
    }

    /// Get the current action by language and secondary language.
    pub fn current_actions(&self) -> Option<(Action, Option<Action>)> {
        self.current_run().map(|raw_ctx| self.get_actions(raw_ctx))
    }

    fn get_actions(&self, raw_ctx: &RawContext) -> (Action, Option<Action>) {
        let action = unwrap_or_default_log!(
            self.context().get_action(&self.settings().lang, raw_ctx),
            "Cannot get action"
        );
        let base_action = self.settings().sub_lang.as_ref().map(|sub_lang| {
            unwrap_or_default_log!(
                self.context().get_action(sub_lang, raw_ctx),
                "Cannot get sub action"
            )
        });
        (action, base_action)
    }

    /// Choose a switch item by index.
    pub fn switch(&mut self, i: usize) {
        log::debug!("Switch {i}");
        self.context_mut().switch(i);
    }

    /// Save current [`ActionRecord`] to the records.
    pub fn save_current_to(&mut self, index: usize) {
        let record = self.current_record.clone();
        if index >= self.records.len() {
            self.records.push(record);
        } else {
            self.records[index] = record;
        }
    }

    /// Save all settings and records.
    pub fn save_settings(&self) -> Result<()> {
        let game = &self.context().game().config.title;
        self.settings_manager.save_settings(self.settings())?;
        self.settings_manager
            .save_global_record(game, self.global_record())?;
        self.settings_manager.save_records(game, self.records())?;
        Ok(())
    }

    /// Determine if current run has been visited.
    pub fn current_visited(&self) -> bool {
        self.current_run()
            .map(|ctx| self.global_record().visited(ctx))
            .unwrap_or_default()
    }

    /// Get the last action text from each record.
    pub fn records_text(&self) -> impl Iterator<Item = ActionText> + '_ {
        self.records().iter().map(|record| {
            let raw_ctx = record
                .last_ctx()
                .expect("there should be at least one RawContext in the ActionRecord");
            let action = unwrap_or_default_log!(
                self.context().get_action(&self.settings().lang, raw_ctx),
                "Cannot get action"
            );
            if let Action::Text(action) = action {
                action
            } else {
                panic!("action in the record should be text action")
            }
        })
    }

    /// Get the current history by language and secondary language.
    pub fn current_history(
        &self,
    ) -> impl DoubleEndedIterator<Item = (Action, Option<Action>)> + '_ {
        self.record()
            .history
            .iter()
            .map(|raw_ctx| self.get_actions(raw_ctx))
    }
}
