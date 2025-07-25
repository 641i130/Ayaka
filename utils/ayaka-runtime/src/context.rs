use crate::{
    plugin::{LoadStatus, Runtime},
    *,
};
use anyhow::{anyhow, bail, Result};
use ayaka_bindings_types::*;
use fallback::Fallback;
use log::error;
use serde::Serialize;
use std::{borrow::Cow, collections::HashMap, future::Future, path::Path, pin::pin, sync::Arc};
use stream_future::{stream, Stream};
use trylog::macros::*;
use vfs::*;
use vfs_tar::TarFS;

/// The game running context.
pub struct Context<M: RawModule + Send + Sync + 'static> {
    game: Game,
    root_path: VfsPath,
    frontend: FrontendType,
    runtime: Arc<Runtime<M>>,
    ctx: RawContext,
    switches: Vec<bool>,
    vars: VarMap,
}

/// The open status when creating [`Context`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "t", content = "data")]
pub enum OpenStatus {
    /// Start loading config file.
    LoadProfile,
    /// Start creating plugin runtime.
    CreateRuntime,
    /// Loading the plugin.
    LoadPlugin(String, usize, usize),
    /// Executing game plugins.
    GamePlugin,
    /// Loading the resources.
    LoadResource,
    /// Loading the paragraphs.
    LoadParagraph,
}

impl From<LoadStatus> for OpenStatus {
    fn from(value: LoadStatus) -> Self {
        match value {
            LoadStatus::CreateEngine => Self::CreateRuntime,
            LoadStatus::LoadPlugin(name, i, len) => Self::LoadPlugin(name, i, len),
        }
    }
}

/// Builder of [`Context`].
pub struct ContextBuilder<M: RawModule + Send + Sync + 'static> {
    frontend: FrontendType,
    linker: M::Linker,
}

impl<M: RawModule + Send + Sync + 'static> ContextBuilder<M> {
    /// Create a new [`ContextBuilder`] with frontend type and plugin runtime linker.
    pub fn new(frontend: FrontendType, linker: M::Linker) -> Self {
        Self { frontend, linker }
    }

    fn open_fs_from_paths(paths: &'_ [impl AsRef<Path>]) -> Result<(VfsPath, Cow<'_, str>)> {
        let (root_path, filename) = if paths.len() == 1 {
            let path = paths[0].as_ref();
            let ext = path.extension().unwrap_or_default();
            if ext == "yaml" {
                let root_path = path
                    .parent()
                    .ok_or_else(|| anyhow!("Cannot get parent from input path."))?;
                (
                    VfsPath::from(PhysicalFS::new(root_path)),
                    path.file_name().unwrap_or_default().to_string_lossy(),
                )
            } else if ext == "ayapack" {
                (TarFS::new_mmap(path)?.into(), "config.yaml".into())
            } else {
                bail!("Cannot determine filesystem.")
            }
        } else {
            let files = paths
                .iter()
                .rev()
                .map(|path| TarFS::new_mmap(path.as_ref()).map(VfsPath::from))
                .collect::<Result<Vec<_>, _>>()?;
            (OverlayFS::new(&files).into(), "config.yaml".into())
        };
        Ok((root_path, filename))
    }

    /// Open a context with config paths.
    ///
    /// If the input `paths` contains only one element, it may be a YAML or an FRFS file.
    /// If the input `paths` contains many element, they should all be FRFS files,
    /// and the latter one will override the former one.
    pub fn with_paths(
        self,
        paths: &'_ [impl AsRef<Path>],
    ) -> Result<ContextBuilderWithPaths<'_, M>> {
        if paths.is_empty() {
            bail!("At least one path should be input.");
        }
        let (root_path, filename) = Self::open_fs_from_paths(paths)?;
        Ok(ContextBuilderWithPaths {
            root_path,
            filename,
            frontend: self.frontend,
            linker: self.linker,
        })
    }

    /// Open a context with config paths.
    pub fn with_vfs(self, paths: &[VfsPath]) -> Result<ContextBuilderWithPaths<'static, M>> {
        if paths.is_empty() {
            bail!("At least one path should be input.");
        }
        Ok(ContextBuilderWithPaths {
            root_path: OverlayFS::new(paths).into(),
            filename: "config.yaml".into(),
            frontend: self.frontend,
            linker: self.linker,
        })
    }
}

/// Builder of [`Context`].
pub struct ContextBuilderWithPaths<'a, M: RawModule + Send + Sync + 'static> {
    root_path: VfsPath,
    filename: Cow<'a, str>,
    frontend: FrontendType,
    linker: M::Linker,
}

impl<'a, M: RawModule + Send + Sync + 'static> ContextBuilderWithPaths<'a, M> {
    /// Open the config and load the [`Context`].
    pub fn open(self) -> impl Future<Output = Result<Context<M>>> + Stream<Item = OpenStatus> + 'a {
        Context::<M>::open(self.root_path, self.filename, self.frontend, self.linker)
    }
}

impl<M: RawModule + Send + Sync + 'static> Context<M> {
    #[stream(OpenStatus, lifetime = 'a)]
    async fn open<'a>(
        root_path: VfsPath,
        filename: impl AsRef<str> + 'a,
        frontend: FrontendType,
        linker: M::Linker,
    ) -> Result<Self> {
        yield OpenStatus::LoadProfile;
        let file = root_path.join(filename.as_ref())?.open_file()?;
        let mut config: GameConfig = serde_yaml::from_reader(file)?;
        let runtime = {
            let runtime = Runtime::load(
                &config.plugins.dir,
                &root_path,
                &config.plugins.modules,
                linker,
            );
            let mut runtime = pin!(runtime);
            while let Some(load_status) = runtime.next().await {
                yield load_status.into();
            }
            runtime.await?
        };

        yield OpenStatus::GamePlugin;
        Self::preprocess_game(&mut config, &runtime)?;

        yield OpenStatus::LoadResource;
        let res = Self::load_resource(&config, &root_path)?;

        yield OpenStatus::LoadParagraph;
        let paras = Self::load_paragraph(&config, &root_path)?;

        Ok(Self {
            game: Game { config, paras, res },
            root_path,
            frontend,
            runtime,
            ctx: RawContext::default(),
            switches: vec![],
            vars: VarMap::default(),
        })
    }

    fn preprocess_game(config: &mut GameConfig, runtime: &Runtime<M>) -> Result<()> {
        for module in runtime.game_modules() {
            let ctx = GameProcessContextRef {
                title: &config.title,
                author: &config.author,
                props: &config.props,
            };
            let res = module.process_game(ctx)?;
            for (key, value) in res.props {
                config.props.insert(key, value);
            }
        }
        Ok(())
    }

    fn load_resource(
        config: &GameConfig,
        root_path: &VfsPath,
    ) -> Result<HashMap<Locale, HashMap<String, RawValue>>> {
        let mut res = HashMap::new();
        if let Some(res_path) = &config.res {
            let res_path = root_path.join(res_path)?;
            for p in res_path.read_dir()? {
                if p.is_file()? && p.extension().unwrap_or_default() == "yaml" {
                    if let Ok(loc) = p
                        .filename()
                        .strip_suffix(".yaml")
                        .unwrap_or_default()
                        .parse::<Locale>()
                    {
                        let r = p.open_file()?;
                        let r = serde_yaml::from_reader(r)?;
                        res.insert(loc, r);
                    }
                }
            }
        }
        Ok(res)
    }

    fn load_paragraph(
        config: &GameConfig,
        root_path: &VfsPath,
    ) -> Result<HashMap<Locale, HashMap<String, Vec<Paragraph>>>> {
        let mut paras = HashMap::new();
        let paras_path = root_path.join(&config.paras)?;
        for p in paras_path.read_dir()? {
            if p.is_dir()? {
                if let Ok(loc) = p.filename().parse::<Locale>() {
                    let mut paras_map = HashMap::new();
                    for p in p.read_dir()? {
                        if p.is_file()? && p.extension().unwrap_or_default() == "yaml" {
                            let key = p
                                .filename()
                                .strip_suffix(".yaml")
                                .unwrap_or_default()
                                .to_string();
                            let para = p.open_file()?;
                            let para = serde_yaml::from_reader(para)?;
                            paras_map.insert(key, para);
                        }
                    }
                    paras.insert(loc, paras_map);
                }
            }
        }
        Ok(paras)
    }

    /// Initialize the [`RawContext`] at the start of the game.
    pub fn set_start_context(&mut self) {
        self.set_context(self.game().start_context())
    }

    /// Initialize the [`RawContext`] with given record.
    pub fn set_context(&mut self, ctx: RawContext) {
        self.ctx = ctx;
    }

    fn current_paragraph(&self, loc: &Locale) -> Option<&Paragraph> {
        self.game
            .find_para(loc, &self.ctx.cur_base_para, &self.ctx.cur_para)
    }

    fn current_paragraph_fallback(&self, loc: &Locale) -> Fallback<&Paragraph> {
        self.game
            .find_para_fallback(loc, &self.ctx.cur_base_para, &self.ctx.cur_para)
    }

    fn current_text(&self, loc: &Locale) -> Option<&Line> {
        self.current_paragraph(loc)
            .and_then(|p| p.texts.get(self.ctx.cur_act))
    }

    fn find_res(&self, loc: &Locale, key: &str) -> Option<&RawValue> {
        self.game
            .find_res_fallback(loc)
            .and_then(|map| map.get(key))
    }

    /// The inner [`Game`] object.
    pub fn game(&self) -> &Game {
        &self.game
    }

    /// The root path of config.
    pub fn root_path(&self) -> &VfsPath {
        &self.root_path
    }

    /// Call the part of script with this context.
    pub fn call(&self, text: &Text) -> Result<String> {
        let mut str = String::new();
        for sub_text in &text.sub_texts {
            let sub_action = self.parse_sub_text(sub_text, None, &self.ctx.locals)?;
            str.push_str(&sub_action.to_string());
        }
        Ok(str.trim().to_string())
    }

    /// Choose a switch item by index, start by 0.
    pub fn switch(&mut self, i: usize) {
        assert!((0..self.switches.len()).contains(&i));
        assert!(self.switches[i]);
        self.ctx
            .locals
            .insert("?".to_string(), RawValue::Num(i as i64));
        for i in 0..self.switches.len() {
            self.ctx.locals.remove(&i.to_string());
        }
    }

    fn parse_text(&self, loc: &Locale, text: &Text, ctx: &RawContext) -> Result<ActionText> {
        let mut action = ActionText::default();
        action.ch_key = text.ch_tag.clone();
        action.character = text.ch_alias.clone().or_else(|| {
            self.find_res(
                loc,
                &format!("ch_{}", action.ch_key.as_deref().unwrap_or_default()),
            )
            .map(|value| value.get_str().into_owned())
        });
        for sub_text in &text.sub_texts {
            let mut sub_action = self.parse_sub_text(sub_text, Some(loc), &ctx.locals)?;
            action.text.append(&mut sub_action.text);
        }
        Ok(action)
    }

    fn parse_sub_text(
        &self,
        sub_text: &SubText,
        loc: Option<&Locale>,
        locals: &VarMap,
    ) -> Result<ActionText> {
        let mut action = ActionText::default();
        match sub_text {
            SubText::Char(c) => action.push_back_chars(c.to_string()),
            SubText::Str(s) => action.push_back_chars(s),
            SubText::Cmd(cmd, args) => {
                let mut arg_strings = vec![];
                for arg in args {
                    let sub_action = self.parse_sub_text(arg, loc, locals)?;
                    arg_strings.push(sub_action.to_string());
                }
                match cmd.as_str() {
                    "res" => {
                        if let Some(loc) = loc {
                            if arg_strings.len() != 1 {
                                log::warn!("Invalid parameter count for `res`: {}", args.len())
                            }
                            if let Some(n) = arg_strings.first() {
                                if let Some(value) = self.find_res(loc, n) {
                                    action.push_back_block(value.get_str())
                                } else {
                                    log::warn!("Cannot find resource {n}");
                                }
                            }
                        }
                    }
                    "var" => {
                        if arg_strings.len() != 1 {
                            log::warn!("Invalid parameter count for `var`: {}", args.len())
                        }
                        if let Some(n) = arg_strings.first() {
                            if let Some(value) = locals.get(n) {
                                action.push_back_block(value.get_str())
                            } else {
                                log::warn!("Cannot find variable {n}")
                            }
                        }
                    }
                    _ => {
                        if let Some(module) = self.runtime.text_module(cmd) {
                            let ctx = TextProcessContextRef {
                                game_props: &self.game.config.props,
                                frontend: self.frontend,
                            };
                            let mut res = module.dispatch_text(cmd, &arg_strings, ctx)?;
                            action.text.append(&mut res.text.text);
                            action.vars.extend(res.text.vars);
                        }
                    }
                }
            }
        }
        Ok(action)
    }

    fn parse_switches(&self, s: &[String]) -> Vec<Switch> {
        s.iter()
            .zip(&self.switches)
            .map(|(item, enabled)| Switch {
                text: item.clone(),
                enabled: *enabled,
            })
            .collect()
    }

    fn process_line(&mut self, t: Line) -> Result<()> {
        match t {
            Line::Empty | Line::Text(_) => {}
            Line::Switch { switches } => {
                self.switches.clear();
                for i in 0..switches.len() {
                    let enabled = self
                        .ctx
                        .locals
                        .get(&i.to_string())
                        .unwrap_or(&RawValue::Unit);
                    let enabled = if let RawValue::Unit = enabled {
                        true
                    } else {
                        enabled.get_bool()
                    };
                    self.switches.push(enabled);
                }
            }
            Line::Custom(props) => {
                self.vars.clear();
                let cmd = props.iter().next().map(|(key, _)| key);
                if let Some(cmd) = cmd {
                    if let Some(module) = self.runtime.line_module(cmd) {
                        let ctx = LineProcessContextRef {
                            game_props: &self.game.config.props,
                            frontend: self.frontend,
                            ctx: &self.ctx,
                            props: &props,
                        };
                        let res = module.dispatch_line(cmd, ctx)?;
                        self.ctx.locals.extend(res.locals);
                        self.vars.extend(res.vars);
                    } else {
                        bail!("Cannot find command {}", cmd)
                    }
                }
            }
        }
        Ok(())
    }

    fn merge_action(&self, action: Fallback<Action>) -> Result<Action> {
        match action.unzip() {
            (None, None) => Ok(Action::default()),
            (Some(action), None) | (None, Some(action)) => Ok(action),
            (Some(action), Some(action_base)) => match (action, action_base) {
                (Action::Text(action), Action::Text(action_base)) => {
                    let action = Fallback::new(Some(action), Some(action_base));
                    let action = action.spec();
                    Ok(Action::Text(ActionText {
                        text: action.text.and_any().unwrap_or_default(),
                        ch_key: action.ch_key.flatten().fallback(),
                        character: action.character.flatten().fallback(),
                        vars: action.vars.and_any().unwrap_or_default(),
                    }))
                }
                (Action::Switches(mut switches), Action::Switches(switches_base)) => {
                    for (item, item_base) in switches.iter_mut().zip(switches_base) {
                        item.enabled = item_base.enabled;
                    }
                    Ok(Action::Switches(switches))
                }
                (Action::Custom(mut vars), Action::Custom(vars_base)) => {
                    vars.extend(vars_base);
                    Ok(Action::Custom(vars))
                }
                _ => bail!("Mismatching action type"),
            },
        }
    }

    fn process_action_text(&self, ctx: &RawContext, action: &mut ActionText) -> Result<()> {
        for module in self.runtime.action_modules() {
            let ctx = ActionProcessContextRef {
                game_props: &self.game.config.props,
                frontend: self.frontend,
                ctx,
                action,
            };
            *action = module.process_action(ctx)?.action;
        }
        while let Some(act) = action.text.back() {
            if act.as_str().trim().is_empty() {
                action.text.pop_back();
            } else {
                break;
            }
        }
        while let Some(act) = action.text.front() {
            if act.as_str().trim().is_empty() {
                action.text.pop_front();
            } else {
                break;
            }
        }
        Ok(())
    }

    /// Get the [`Action`] from [`Locale`] and [`RawContext`].
    pub fn get_action(&self, loc: &Locale, ctx: &RawContext) -> Result<Action> {
        let cur_text = self
            .game
            .find_para_fallback(loc, &ctx.cur_base_para, &ctx.cur_para)
            .map(|p| p.texts.get(ctx.cur_act))
            .flatten();

        let action = cur_text
            .map(|t| match t {
                Line::Text(t) => self.parse_text(loc, t, ctx).map(Action::Text).ok(),
                Line::Switch { switches } => Some(Action::Switches(self.parse_switches(switches))),
                // The real vars will be filled in `merge_action`.
                Line::Custom(_) => Some(Action::Custom(self.vars.clone())),
                _ => None,
            })
            .flatten();

        let mut act = self.merge_action(action)?;
        if let Action::Text(act) = &mut act {
            self.process_action_text(ctx, act)?;
        }
        Ok(act)
    }

    /// Step to next line.
    pub fn next_run(&mut self) -> Option<RawContext> {
        let cur_text_base = loop {
            let cur_para = self.current_paragraph(&self.game.config.base_lang);
            let cur_text = self.current_text(&self.game.config.base_lang);
            match (cur_para.is_some(), cur_text.is_some()) {
                (true, true) => break cur_text,
                (true, false) => {
                    self.ctx.cur_para = cur_para
                        .and_then(|p| p.next.as_ref())
                        .map(|text| unwrap_or_default_log!(self.call(text), "Cannot get next para"))
                        .unwrap_or_default();
                    self.ctx.cur_act = 0;
                }
                (false, _) => {
                    if self.ctx.cur_base_para == self.ctx.cur_para {
                        if !self.ctx.cur_para.is_empty() {
                            error!(
                                "Cannot find paragraph \"{}\"",
                                self.ctx.cur_para.escape_default()
                            );
                        }
                        return None;
                    } else {
                        self.ctx.cur_base_para = self.ctx.cur_para.clone();
                    }
                }
            }
        };

        let ctx = cur_text_base.cloned().map(|t| {
            unwrap_or_default_log!(self.process_line(t), "Parse line error");
            self.ctx.clone()
        });
        self.ctx.cur_act += 1;
        ctx
    }

    /// Get current paragraph title.
    pub fn current_paragraph_title(&self, loc: &Locale) -> Option<&String> {
        self.current_paragraph_fallback(loc)
            .and_then(|p| p.title.as_ref())
    }
}
