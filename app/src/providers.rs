//! System search integration: results in GNOME Shell's overview and in KDE's KRunner.
//! Both call into the app over D-Bus; the app may be started just to answer them.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

use adw::prelude::*;
use filefind_core::{Filters, Hit};
use gtk::{gio, glib};

use crate::backend::Backend;
use crate::i18n::tr;
use crate::window::display_path;

pub const GNOME_PATH: &str = "/io/github/pluja/Filefind/SearchProvider";
pub const KRUNNER_PATH: &str = "/runner";
const RESULTS: usize = 10;

const GNOME_XML: &str = r#"<node>
  <interface name="org.gnome.Shell.SearchProvider2">
    <method name="GetInitialResultSet"><arg type="as" direction="in"/><arg type="as" direction="out"/></method>
    <method name="GetSubsearchResultSet"><arg type="as" direction="in"/><arg type="as" direction="in"/><arg type="as" direction="out"/></method>
    <method name="GetResultMetas"><arg type="as" direction="in"/><arg type="aa{sv}" direction="out"/></method>
    <method name="ActivateResult"><arg type="s" direction="in"/><arg type="as" direction="in"/><arg type="u" direction="in"/></method>
    <method name="LaunchSearch"><arg type="as" direction="in"/><arg type="u" direction="in"/></method>
  </interface>
</node>"#;

const KRUNNER_XML: &str = r#"<node>
  <interface name="org.kde.krunner1">
    <method name="Actions"><arg type="a(sss)" direction="out"/></method>
    <method name="Match"><arg type="s" direction="in"/><arg type="a(sssida{sv})" direction="out"/></method>
    <method name="Run"><arg type="s" direction="in"/><arg type="s" direction="in"/></method>
    <method name="Teardown"/>
    <method name="SetActivationToken"><arg type="s" direction="in"/></method>
  </interface>
</node>"#;

type KRunnerMatch = (String, String, String, i32, f64, HashMap<String, glib::Variant>);

struct Providers {
    app: glib::WeakRef<adw::Application>,
    backend: Rc<Backend>,
    /// Hits from the latest search, so GNOME Shell can ask for their details.
    recent: RefCell<HashMap<String, Hit>>,
}

pub fn register(app: &adw::Application, backend: &Rc<Backend>) {
    let Some(connection) = app.dbus_connection() else { return };
    let providers = Rc::new(Providers { app: app.downgrade(), backend: backend.clone(), recent: RefCell::default() });
    for (path, xml) in [(GNOME_PATH, GNOME_XML), (KRUNNER_PATH, KRUNNER_XML)] {
        let interface = gio::DBusNodeInfo::for_xml(xml).ok().and_then(|node| node.interfaces().first().cloned());
        let Some(interface) = interface else { continue };
        let providers = providers.clone();
        let registered = connection
            .register_object(path, &interface)
            .method_call(move |_, _, _, _, method, params, invocation| providers.call(method, params, invocation))
            .build();
        if let Err(e) = registered {
            log::warn!("cannot register {path}: {e}");
        }
    }
}

impl Providers {
    fn call(self: &Rc<Self>, method: &str, params: glib::Variant, invocation: gio::DBusMethodInvocation) {
        // Answering keeps the app alive; it exits after a while once no one is asking.
        let hold = self.app.upgrade().map(|app| app.hold());
        let this = self.clone();
        let method = method.to_owned();
        glib::spawn_future_local(async move {
            match this.reply(&method, &params).await {
                Ok(reply) => invocation.return_value(reply.as_ref()),
                Err(()) => invocation.return_dbus_error("org.freedesktop.DBus.Error.InvalidArgs", "unexpected arguments"),
            }
            drop(hold);
        });
    }

    /// The reply for `method`, or `Err` if its arguments are malformed.
    async fn reply(&self, method: &str, params: &glib::Variant) -> Result<Option<glib::Variant>, ()> {
        let reply = match method {
            "GetInitialResultSet" => {
                let (terms,) = params.get::<(Vec<String>,)>().ok_or(())?;
                Some((self.search(&terms.join(" ")).await,).to_variant())
            }
            "GetSubsearchResultSet" => {
                let (_, terms) = params.get::<(Vec<String>, Vec<String>)>().ok_or(())?;
                Some((self.search(&terms.join(" ")).await,).to_variant())
            }
            "GetResultMetas" => {
                let (ids,) = params.get::<(Vec<String>,)>().ok_or(())?;
                let metas: Vec<HashMap<String, glib::Variant>> = ids.iter().map(|id| self.meta(id)).collect();
                Some((metas,).to_variant())
            }
            "ActivateResult" => {
                let (id, _, _) = params.get::<(String, Vec<String>, u32)>().ok_or(())?;
                if self.is_result(&id) {
                    crate::launch::open(&id, None, || {});
                }
                None
            }
            "LaunchSearch" => {
                let (terms, _) = params.get::<(Vec<String>, u32)>().ok_or(())?;
                self.show_in_app(&terms.join(" "));
                None
            }
            "Actions" => {
                let actions = vec![("show-in-folder".to_owned(), tr("Show in Folder"), "folder-open-symbolic".to_owned())];
                Some((actions,).to_variant())
            }
            "Match" => {
                let (query,) = params.get::<(String,)>().ok_or(())?;
                Some((self.krunner_matches(&query).await,).to_variant())
            }
            "Run" => {
                let (id, action) = params.get::<(String, String)>().ok_or(())?;
                if self.is_result(&id) {
                    match action.as_str() {
                        "show-in-folder" => crate::launch::show_in_folder(&id, None, || {}),
                        _ => crate::launch::open(&id, None, || {}),
                    }
                }
                None
            }
            _ => None,
        };
        Ok(reply)
    }

    /// Only files this provider just returned may be opened on a caller's behalf.
    fn is_result(&self, id: &str) -> bool {
        self.recent.borrow().contains_key(id)
    }

    async fn hits(&self, query: &str) -> Vec<Hit> {
        let mut request = self.backend.request(query, Filters::default());
        request.limit = RESULTS;
        request.sort = filefind_core::Sort::Relevance;
        if request.is_empty() {
            return Vec::new();
        }
        let hits = self.backend.search(request).await.hits;
        self.recent.replace(hits.iter().map(|h| (h.path.clone(), h.clone())).collect());
        hits
    }

    async fn search(&self, query: &str) -> Vec<String> {
        self.hits(query).await.into_iter().map(|h| h.path).collect()
    }

    fn meta(&self, id: &str) -> HashMap<String, glib::Variant> {
        let path = Path::new(id);
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| id.to_owned());
        let description = self.recent.borrow().get(id).map(describe).unwrap_or_else(|| folder_of(path));
        let (content_type, _) = gio::content_type_guess(Some(path), None::<&[u8]>);
        let icon = gio::content_type_get_icon(&content_type);
        let mut meta = HashMap::new();
        meta.insert("id".to_owned(), id.to_variant());
        meta.insert("name".to_owned(), name.to_variant());
        meta.insert("description".to_owned(), description.to_variant());
        if let Some(icon) = IconExt::to_string(&icon) {
            meta.insert("gicon".to_owned(), icon.to_variant());
        }
        meta
    }

    async fn krunner_matches(&self, query: &str) -> Vec<KRunnerMatch> {
        const MODERATE: i32 = 50;
        self.hits(query)
            .await
            .iter()
            .enumerate()
            .map(|(i, hit)| {
                let path = Path::new(&hit.path);
                let (content_type, _) = gio::content_type_guess(Some(path), None::<&[u8]>);
                let icon = gio::content_type_get_generic_icon_name(&content_type).map(|s| s.to_string()).unwrap_or_default();
                let mut props = HashMap::new();
                props.insert("subtext".to_owned(), describe(hit).to_variant());
                props.insert("urls".to_owned(), vec![gio::File::for_path(path).uri().to_string()].to_variant());
                props.insert("actions".to_owned(), vec!["show-in-folder".to_owned()].to_variant());
                let name: String = hit.name.iter().map(|(s, _)| s.as_str()).collect();
                (hit.path.clone(), name, icon, MODERATE, (1.0 - i as f64 * 0.05).max(0.1), props)
            })
            .collect()
    }

    fn show_in_app(&self, query: &str) {
        if let Some(app) = self.app.upgrade() {
            crate::window::present(&app, &self.backend, Some(query));
        }
    }
}

fn folder_of(path: &Path) -> String {
    path.parent().map(display_path).unwrap_or_default()
}

/// The snippet if the file matched by content, otherwise its folder.
fn describe(hit: &Hit) -> String {
    let snippet: String = hit.snippet.iter().map(|(s, _)| s.as_str()).collect();
    if snippet.trim().is_empty() {
        folder_of(Path::new(&hit.path))
    } else {
        snippet
    }
}
