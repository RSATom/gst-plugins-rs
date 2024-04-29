// Copyright (C) 2022 OneStream Live <guillaume.desmottes@onestream.live>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * tracer-pipeline-snapshot:
 *
 * This tracer provides an easy way to take a snapshot of all the pipelines without
 * having to modify the application.
 * One just have to load the tracer and send the `SIGUSR1` UNIX signal to take snapshots.
 * It currently only works on UNIX systems.
 *
 * When taking a snapshot pipelines are saved to DOT files, but the tracer may be
 * extended in the future to dump more information.
 *
 * Example:
 *
 * ```console
 * $ GST_TRACERS="pipeline-snapshot" GST_DEBUG_DUMP_DOT_DIR=. gst-launch-1.0 audiotestsrc ! fakesink
 * ```
 * You can then trigger a snapshot using:
 * ```console
 * $ kill -SIGUSR1 $(pidof gst-launch-1.0)
 * ```
 *
 * Parameters can be passed to configure the tracer:
 * - `dot-dir` (string, default: None): directory where to place dot files (overriding `GST_DEBUG_DUMP_DOT_DIR`). Set to `xdg-cache` to use the XDG cache directory.
 * - `dot-prefix` (string, default: "pipeline-snapshot-"): when dumping pipelines to a `dot` file each file is named `$prefix$pipeline_name.dot`.
 * - `dot-ts` (boolean, default: "true"): if the current timestamp should be added as a prefix to each pipeline `dot` file.
 *
 * Example:
 *
 * ```console
 * $ GST_TRACERS="pipeline-snapshot(dot-prefix="badger-",dot-ts=false)" GST_DEBUG_DUMP_DOT_DIR=. gst-launch-1.0 audiotestsrc ! fakesink
 * ```
 */
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex, RwLock};

use gst::glib;
use gst::glib::translate::ToGlibPtr;
use gst::glib::Properties;
use gst::prelude::*;
use gst::subclass::prelude::*;
use std::sync::LazyLock;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "pipeline-snapshot",
        gst::DebugColorFlags::empty(),
        Some("pipeline snapshot tracer"),
    )
});

static START_TIME: LazyLock<gst::ClockTime> = LazyLock::new(gst::get_timestamp);

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Copy, Clone)]
struct ElementPtr(std::ptr::NonNull<gst::ffi::GstElement>);

unsafe impl Send for ElementPtr {}
unsafe impl Sync for ElementPtr {}

impl ElementPtr {
    fn from_ref(element: &gst::Element) -> Self {
        let p = element.to_glib_none().0;
        Self(std::ptr::NonNull::new(p).unwrap())
    }

    fn from_object_ptr(p: std::ptr::NonNull<gst::ffi::GstObject>) -> Self {
        let p = p.cast();
        Self(p)
    }
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstPipelineSnapshotCleanupMode")]
#[non_exhaustive]
pub enum CleanupMode {
    #[enum_value(
        name = "CleanupInitial: Remove all .dot files from folder when starting",
        nick = "initial"
    )]
    Initial,
    #[enum_value(
        name = "CleanupAutomatic: cleanup .dot files before each snapshots if pipeline-snapshot::folder-mode is not None \
                otherwise cleanup `.dot` files in folders",
        nick = "automatic"
    )]
    Automatic,
    #[enum_value(name = "None: Never remove any dot file", nick = "none")]
    None,
}

impl std::str::FromStr for CleanupMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "initial" => Ok(CleanupMode::Initial),
            "automatic" => Ok(CleanupMode::Automatic),
            "none" => Ok(CleanupMode::None),
            _ => Err(format!("unknown cleanup mode: {}", s)),
        }
    }
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstPipelineSnapshotFolderMode")]
#[non_exhaustive]
pub enum FolderMode {
    #[enum_value(name = "None: Do not use folders to store dot files", nick = "none")]
    None,
    #[enum_value(
        name = "Numbered: Use folders to store dot files, each time `.snapshot()` is called a new folder is created \
                and named with a number starting from 0.",
        nick = "numbered"
    )]
    Numbered,
    #[enum_value(
        name = "Timed: Use folders to store dot files, each time `.snapshot()` is called a new folder is created \
                         and named with the current timestamp.",
        nick = "timed"
    )]
    Timed,
}

impl std::str::FromStr for FolderMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "none" => Ok(FolderMode::None),
            "numbered" => Ok(FolderMode::Numbered),
            "timed" => Ok(FolderMode::Timed),
            _ => Err(format!("unknown folder mode: {}", s)),
        }
    }
}

#[derive(Debug)]
struct Settings {
    dot_prefix: Option<String>,
    dot_ts: bool,
    dot_pipeline_ptr: bool,
    dot_dir: Option<String>,
    cleanup_mode: CleanupMode,
    folder_mode: FolderMode,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            dot_dir: None,
            dot_prefix: Some("pipeline-snapshot-".to_string()),
            dot_ts: true,
            cleanup_mode: CleanupMode::None,
            dot_pipeline_ptr: false,
            folder_mode: FolderMode::None,
        }
    }
}

impl Settings {
    fn set_dot_dir(&mut self, dot_dir: Option<String>) {
        if let Some(dot_dir) = dot_dir {
            if dot_dir == "xdg-cache" {
                let mut path = dirs::cache_dir().expect("Failed to find cache directory");
                path.push("gstreamer-dots");
                self.dot_dir = path.to_str().map(|s| s.to_string());
            } else {
                self.dot_dir = Some(dot_dir);
            }
        } else {
            self.dot_dir = std::env::var("GST_DEBUG_DUMP_DOT_DIR").ok();
        }
    }

    fn update_from_params(&mut self, imp: &PipelineSnapshot, params: String) {
        let s = match gst::Structure::from_str(&format!("pipeline-snapshot,{params}")) {
            Ok(s) => s,
            Err(err) => {
                gst::warning!(CAT, imp = imp, "failed to parse tracer parameters: {}", err);
                return;
            }
        };

        if let Ok(dot_dir) = s.get("dot-dir") {
            self.set_dot_dir(dot_dir);
            gst::log!(CAT, imp = imp, "dot-prefix = {:?}", self.dot_dir);
        }

        if let Ok(dot_prefix) = s.get("dot-prefix") {
            gst::log!(CAT, imp = imp, "dot-prefix = {:?}", dot_prefix);
            self.dot_prefix = dot_prefix;
        }

        if let Ok(dot_ts) = s.get("dot-ts") {
            gst::log!(CAT, imp = imp, "dot-ts = {}", dot_ts);
            self.dot_ts = dot_ts;
        }

        if let Ok(dot_pipeline_ptr) = s.get("dot-pipeline-ptr") {
            gst::log!(CAT, imp = imp, "dot-pipeline-ptr = {}", dot_pipeline_ptr);
            self.dot_pipeline_ptr = dot_pipeline_ptr;
        }

        if let Ok(cleanup_mod) = s.get::<&str>("cleanup-mode") {
            self.cleanup_mode = match cleanup_mod.parse() {
                Ok(mode) => mode,
                Err(err) => {
                    gst::warning!(CAT, imp = imp, "unknown cleanup-mode: {}", err);
                    CleanupMode::None
                }
            };
        }

        if let Ok(folder_mode) = s.get::<&str>("folder-mode") {
            self.folder_mode = match folder_mode.parse() {
                Ok(mode) => mode,
                Err(err) => {
                    gst::warning!(CAT, imp = imp, "unknown folder-mode: {}", err);
                    FolderMode::None
                }
            };
        }
    }
}

#[derive(Debug, Default)]
struct State {
    current_folder: u32,
    pipelines: HashMap<ElementPtr, glib::WeakRef<gst::Element>>,
}

#[derive(Properties, Debug, Default)]
#[properties(wrapper_type = super::PipelineSnapshot)]
pub struct PipelineSnapshot {
    #[property(name="dot-dir", get, set = Self::set_dot_dir, construct_only, type = String, member = dot_dir, blurb = "Directory where to place dot files")]
    #[property(name="dot-prefix", get, set, type = String, member = dot_prefix, blurb = "Prefix for dot files")]
    #[property(name="dot-ts", get, set, type = bool, member = dot_ts, blurb = "Add timestamp to dot files")]
    #[property(name="dot-pipeline-ptr", get, set, type = bool, member = dot_pipeline_ptr, blurb = "Add pipeline ptr value to dot files")]
    #[property(name="cleanup-mode", get  = |s: &Self| s.settings.read().unwrap().cleanup_mode, set, type = CleanupMode, member = cleanup_mode, blurb = "Cleanup mode", builder(CleanupMode::None))]
    #[property(name="folder-mode",
               get=|s: &Self| s.settings.read().unwrap().folder_mode,
               set,
               type = FolderMode, member = folder_mode, blurb = "How to create folder each time a snapshot of all pipelines is made", builder(FolderMode::None))]
    settings: RwLock<Settings>,
    handles: Mutex<Option<Handles>>,
    state: Arc<Mutex<State>>,
}

#[derive(Debug)]
struct Handles {
    #[cfg(unix)]
    signal: signal_hook::iterator::Handle,
    thread: std::thread::JoinHandle<()>,
}

#[glib::object_subclass]
impl ObjectSubclass for PipelineSnapshot {
    const NAME: &'static str = "GstPipelineSnapshot";
    type Type = super::PipelineSnapshot;
    type ParentType = gst::Tracer;
}

#[glib::derived_properties]
impl ObjectImpl for PipelineSnapshot {
    fn constructed(&self) {
        let _ = START_TIME.as_ref();
        self.parent_constructed();

        let mut settings = self.settings.write().unwrap();
        if let Some(params) = self.obj().property::<Option<String>>("params") {
            settings.update_from_params(self, params);
        }

        if settings.cleanup_mode == CleanupMode::Initial {
            drop(settings);
            self.cleanup_dots(&self.settings.read().unwrap().dot_dir.as_ref(), true);
        }

        self.register_hook(TracerHook::ElementNew);
        self.register_hook(TracerHook::ObjectDestroyed);

        if let Err(err) = self.setup_signal() {
            gst::warning!(CAT, imp = self, "failed to setup UNIX signals: {}", err);
        }
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: LazyLock<Vec<glib::subclass::Signal>> = LazyLock::new(|| {
            vec![glib::subclass::Signal::builder("snapshot")
                .action()
                .class_handler(|_, args| {
                    args[0].get::<super::PipelineSnapshot>().unwrap().snapshot();

                    None
                })
                .build()]
        });

        SIGNALS.as_ref()
    }

    fn dispose(&self) {
        let mut handles = self.handles.lock().unwrap();
        if let Some(handles) = handles.take() {
            #[cfg(unix)]
            handles.signal.close();
            handles.thread.join().unwrap();
        }
    }
}

impl GstObjectImpl for PipelineSnapshot {}

impl TracerImpl for PipelineSnapshot {
    fn element_new(&self, _ts: u64, element: &gst::Element) {
        if element.is::<gst::Pipeline>() {
            let pipeline_ptr = ElementPtr::from_ref(element);

            let weak = element.downgrade();
            let mut state = self.state.lock().unwrap();
            state.pipelines.insert(pipeline_ptr, weak);
            gst::debug!(
                CAT,
                imp = self,
                "new pipeline: {} ({:?}) got {} now",
                element.name(),
                pipeline_ptr,
                state.pipelines.len()
            );
        }
    }

    fn object_destroyed(&self, _ts: u64, object: std::ptr::NonNull<gst::ffi::GstObject>) {
        let mut state = self.state.lock().unwrap();
        let object = ElementPtr::from_object_ptr(object);
        if state.pipelines.remove(&object).is_some() {
            gst::debug!(
                CAT,
                imp = self,
                "Pipeline removed: {:?} - {} remaining",
                object,
                state.pipelines.len()
            );
        }
    }
}

impl PipelineSnapshot {
    fn set_dot_dir(&self, dot_dir: Option<String>) {
        let mut settings = self.settings.write().unwrap();
        settings.set_dot_dir(dot_dir);
    }

    pub(crate) fn snapshot(&self) {
        let settings = self.settings.read().unwrap();

        let dot_dir = if let Some(dot_dir) = settings.dot_dir.as_ref() {
            if !matches!(settings.folder_mode, FolderMode::None) {
                let dot_dir = match settings.folder_mode {
                    FolderMode::Numbered => {
                        let mut state = self.state.lock().unwrap();
                        let res = state.current_folder;
                        state.current_folder += 1;

                        format!("{dot_dir}/{res}")
                    }
                    FolderMode::Timed => {
                        let datetime: chrono::DateTime<chrono::Local> = chrono::Local::now();
                        format!("{dot_dir}/{}", datetime.format("%Y-%m-%d %H:%M:%S"))
                    }
                    _ => unreachable!(),
                };

                if let Err(err) = std::fs::create_dir_all(&dot_dir) {
                    gst::warning!(
                        CAT,
                        imp = self,
                        "Failed to create folder {}: {}",
                        dot_dir,
                        err
                    );
                    return;
                }

                dot_dir
            } else {
                dot_dir.clone()
            }
        } else {
            gst::info!(CAT, imp = self, "No dot-dir set, not dumping pipelines");
            return;
        };

        if matches!(settings.cleanup_mode, CleanupMode::Automatic) {
            self.cleanup_dots(&Some(&dot_dir), false);
        }

        let ts = if settings.dot_ts {
            format!("{:?}-", gst::get_timestamp() - *START_TIME)
        } else {
            "".to_string()
        };

        let pipelines = {
            let state = self.state.lock().unwrap();
            gst::log!(
                CAT,
                imp = self,
                "dumping {} pipelines",
                state.pipelines.len()
            );

            state
                .pipelines
                .iter()
                .filter_map(|(ptr, w)| {
                    let pipeline = w.upgrade();

                    if pipeline.is_none() {
                        gst::warning!(CAT, imp = self, "Pipeline {ptr:?} disappeared");
                    }
                    pipeline
                })
                .collect::<Vec<_>>()
        };

        for pipeline in pipelines.into_iter() {
            let pipeline = pipeline.downcast::<gst::Pipeline>().unwrap();
            gst::debug!(CAT, imp = self, "dump {}", pipeline.name());

            let pipeline_ptr = if settings.dot_pipeline_ptr {
                let pipeline_ptr: *const gst::ffi::GstPipeline = pipeline.to_glib_none().0;

                format!("-{:?}", pipeline_ptr)
            } else {
                "".to_string()
            };
            let dot_path = format!(
                "{dot_dir}/{ts}{}{}{pipeline_ptr}.dot",
                settings.dot_prefix.as_ref().map_or("", |s| s.as_str()),
                pipeline.name(),
            );
            gst::debug!(CAT, imp = self, "Writing {}", dot_path);
            match std::fs::File::create(&dot_path) {
                Ok(mut f) => {
                    let data = pipeline.debug_to_dot_data(gst::DebugGraphDetails::all());
                    if let Err(e) = f.write_all(data.as_bytes()) {
                        gst::warning!(CAT, imp = self, "Failed to write {}: {}", dot_path, e);
                    }
                }
                Err(e) => {
                    gst::warning!(CAT, imp = self, "Failed to create {}: {}", dot_path, e);
                }
            }
        }
    }

    #[cfg(unix)]
    fn setup_signal(&self) -> anyhow::Result<()> {
        use signal_hook::consts::signal::*;
        use signal_hook::iterator::Signals;

        let mut signals = Signals::new([SIGUSR1])?;
        let signal_handle = signals.handle();

        let this_weak = self.obj().downgrade();
        let thread_handle = std::thread::spawn(move || {
            for signal in &mut signals {
                match signal {
                    SIGUSR1 => {
                        if let Some(this) = this_weak.upgrade() {
                            this.snapshot();
                        } else {
                            break;
                        };
                    }
                    _ => unreachable!(),
                }
            }
        });

        let mut handles = self.handles.lock().unwrap();
        *handles = Some(Handles {
            signal: signal_handle,
            thread: thread_handle,
        });

        Ok(())
    }

    #[cfg(not(unix))]
    fn setup_signal(&self) -> anyhow::Result<()> {
        anyhow::bail!("only supported on UNIX system");
    }

    fn cleanup_dots(&self, dot_dir: &Option<&String>, recurse: bool) {
        if let Some(dot_dir) = dot_dir {
            gst::info!(CAT, imp = self, "Cleaning up {}", dot_dir);
            let mut paths = match std::fs::read_dir(dot_dir) {
                Ok(entries) => {
                    entries
                        .filter_map(|entry| {
                            let entry = entry.ok()?; // Handle possible errors when reading directory entries
                            let path = entry.path();
                            let extension = path.extension()?.to_str()?; // Get the extension as a string
                            if extension.ends_with(".dot") {
                                Some(path.to_path_buf())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<PathBuf>>()
                }
                Err(e) => {
                    gst::warning!(CAT, imp = self, "Failed to read {}: {}", dot_dir, e);
                    return;
                }
            };

            if recurse {
                paths.append(
                    &mut walkdir::WalkDir::new(dot_dir)
                        .into_iter()
                        .filter_map(|entry| {
                            let entry = entry.ok()?;
                            let path = entry.path();
                            let extension = path.extension()?.to_str()?;
                            if extension == "dot" {
                                Some(path.to_path_buf())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<PathBuf>>(),
                )
            }

            for path in paths {
                if let Err(e) = std::fs::remove_file(&path) {
                    gst::warning!(
                        CAT,
                        imp = self,
                        "Failed to remove {}: {}",
                        path.display(),
                        e
                    );
                }
            }
        }
    }
}
