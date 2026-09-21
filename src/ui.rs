use crate::download::{download_deb_set, Progress};
use crate::install::{run_privileged_install, run_privileged_remove};
use crate::system::{
    compare_to_running, format_bytes, query_system, SystemInfo, VersionRelation,
    MIN_BOOT_BYTES, MIN_ROOT_BYTES,
};
use crate::versions::{fetch_deb_list, fetch_versions, KernelVersion};

use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, Label, ListBox, Orientation, ProgressBar,
    ScrolledWindow, SelectionMode, Spinner, Stack, Switch, TextView, WrapMode,
};
use libadwaita::prelude::*;
use libadwaita::{
    AboutDialog, ActionRow, AlertDialog, Application, ApplicationWindow, Banner, HeaderBar,
    PreferencesGroup, Toast, ToastOverlay,
};

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

// ─────────────────────────────────────────────────────────────────────────────
//  App state
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct AppState {
    versions: Vec<KernelVersion>,
    selected_version: Option<KernelVersion>,
    /// Directory of verified downloaded debs, ready to install.
    staged_dir: Option<PathBuf>,
    staged_version: Option<String>,
    staged_files: Vec<(String, bool)>, // (filename, checksum_verified)
    sysinfo: SystemInfo,
    download_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// RC-visibility toggle on the Browse tab. Off by default.
    show_rc: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
//  Async bridge: run future on Tokio, deliver result to GTK main thread
// ─────────────────────────────────────────────────────────────────────────────

fn spawn_async<F, T, CB>(future: F, callback: CB)
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
    CB: FnOnce(T) + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel::<T>();
    crate::runtime().spawn(async move {
        let _ = tx.send(future.await);
    });
    glib::MainContext::default().spawn_local(async move {
        if let Ok(val) = rx.await {
            callback(val);
        }
    });
}

// ─────────────────────────────────────────────────────────────────────────────
//  build_ui
// ─────────────────────────────────────────────────────────────────────────────

pub fn build_ui(app: &Application) {
    let state = Rc::new(RefCell::new(AppState::default()));

    let window = ApplicationWindow::builder()
        .application(app)
        .title("KernelPop")
        .default_width(780)
        .default_height(660)
        .build();

    let toast_overlay = ToastOverlay::new();

    // ── Header ───────────────────────────────────────────────────────────────
    let header = HeaderBar::new();

    let refresh_btn = Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Refresh kernel list")
        .build();
    header.pack_start(&refresh_btn);

    let about_btn = Button::builder()
        .icon_name("help-about-symbolic")
        .tooltip_text("About")
        .build();
    header.pack_end(&about_btn);

    // ── Stack ────────────────────────────────────────────────────────────────
    let stack = Stack::new();
    stack.set_transition_type(gtk4::StackTransitionType::SlideLeftRight);
    let switcher = gtk4::StackSwitcher::builder().stack(&stack).build();
    header.set_title_widget(Some(&switcher));

    // ═════════════════════════════════════════════════════════════════════════
    //  PAGE 1: System — installed kernels and their boot health
    // ═════════════════════════════════════════════════════════════════════════
    let sysinfo_page = GtkBox::new(Orientation::Vertical, 12);
    sysinfo_page.set_margin_top(12);
    sysinfo_page.set_margin_bottom(12);
    sysinfo_page.set_margin_start(12);
    sysinfo_page.set_margin_end(12);

    // "Newer kernel available" banner — populated by a background mainline
    // fetch after the (local, fast) system query renders; see load_sysinfo.
    let update_banner = Banner::new("");
    update_banner.set_revealed(false);
    sysinfo_page.append(&update_banner);

    let running_group = PreferencesGroup::builder().title("Running Kernel").build();
    let running_row = ActionRow::builder().title("Detecting…").build();
    running_group.add(&running_row);
    sysinfo_page.append(&running_group);

    let kernels_group = PreferencesGroup::builder()
        .title("Installed Kernels")
        .description("Every kernel in /boot, checked for a matching initramfs and modules directory")
        .build();
    let kernels_list = ListBox::new();
    kernels_list.set_selection_mode(SelectionMode::None);
    kernels_list.add_css_class("boxed-list");
    kernels_group.add(&kernels_list);
    sysinfo_page.append(&kernels_group);

    let disk_group = PreferencesGroup::builder().title("Disk").build();
    let boot_row = ActionRow::builder().title("/boot Free Space").subtitle("Checking…").build();
    let root_row = ActionRow::builder().title("/ Free Space").subtitle("Checking…").build();
    disk_group.add(&boot_row);
    disk_group.add(&root_row);
    sysinfo_page.append(&disk_group);

    let refresh_sysinfo_btn = Button::builder()
        .label("Refresh System Info")
        .halign(Align::Center)
        .margin_top(8)
        .build();
    refresh_sysinfo_btn.add_css_class("flat");
    sysinfo_page.append(&refresh_sysinfo_btn);

    let sys_scroll = ScrolledWindow::builder().vexpand(true).child(&sysinfo_page).build();
    stack.add_titled(&sys_scroll, Some("system"), "System");

    // ═════════════════════════════════════════════════════════════════════════
    //  PAGE 2: Browse — mainline versions
    // ═════════════════════════════════════════════════════════════════════════
    let browse_page = GtkBox::new(Orientation::Vertical, 0);

    let banner = Banner::new("");
    banner.set_revealed(false);
    browse_page.append(&banner);

    let search_bar = gtk4::SearchBar::new();
    let search_entry = gtk4::SearchEntry::new();
    search_entry.set_placeholder_text(Some("Filter versions…"));
    search_bar.set_child(Some(&search_entry));
    search_bar.set_show_close_button(false);
    search_bar.set_search_mode(true);
    browse_page.append(&search_bar);

    let rc_toggle_row = GtkBox::new(Orientation::Horizontal, 8);
    rc_toggle_row.set_margin_start(12);
    rc_toggle_row.set_margin_end(12);
    rc_toggle_row.set_margin_bottom(4);
    rc_toggle_row.set_halign(Align::End);
    let rc_toggle_label = Label::new(Some("Show release candidates"));
    rc_toggle_label.add_css_class("dim-label");
    let rc_toggle = Switch::new();
    rc_toggle.set_active(false);
    rc_toggle.set_valign(Align::Center);
    rc_toggle_row.append(&rc_toggle_label);
    rc_toggle_row.append(&rc_toggle);
    browse_page.append(&rc_toggle_row);

    let list_box = ListBox::new();
    list_box.set_selection_mode(SelectionMode::Single);
    list_box.add_css_class("boxed-list");
    list_box.set_margin_top(8);
    list_box.set_margin_bottom(8);
    list_box.set_margin_start(12);
    list_box.set_margin_end(12);
    let scroll = ScrolledWindow::builder().vexpand(true).child(&list_box).build();
    browse_page.append(&scroll);

    let list_spinner = Spinner::new();
    list_spinner.set_halign(Align::Center);
    list_spinner.set_size_request(48, 48);
    browse_page.append(&list_spinner);

    let selected_label = Label::builder()
        .label("No version selected")
        .margin_top(4)
        .margin_bottom(4)
        .build();
    selected_label.add_css_class("dim-label");
    browse_page.append(&selected_label);

    let browse_bar = gtk4::ActionBar::new();
    let download_btn = Button::builder().label("Download").sensitive(false).build();
    download_btn.add_css_class("suggested-action");
    browse_bar.pack_end(&download_btn);
    browse_page.append(&browse_bar);

    stack.add_titled(&browse_page, Some("browse"), "Browse");

    // ═════════════════════════════════════════════════════════════════════════
    //  PAGE 3: Install — staged packages and install action
    // ═════════════════════════════════════════════════════════════════════════
    let install_page = GtkBox::new(Orientation::Vertical, 12);
    install_page.set_margin_top(12);
    install_page.set_margin_bottom(12);
    install_page.set_margin_start(12);
    install_page.set_margin_end(12);

    let staged_group = PreferencesGroup::builder().title("Staged Kernel").build();
    let staged_row = ActionRow::builder()
        .title("Nothing staged")
        .subtitle("Go to Browse to download a kernel")
        .build();
    let status_row = ActionRow::builder().title("Version Status").subtitle("—").build();
    staged_group.add(&staged_row);
    staged_group.add(&status_row);
    install_page.append(&staged_group);

    let files_group = PreferencesGroup::builder()
        .title("Packages")
        .description("Verified against the published CHECKSUMS where available")
        .build();
    let files_list = ListBox::new();
    files_list.set_selection_mode(SelectionMode::None);
    files_list.add_css_class("boxed-list");
    files_group.add(&files_list);
    install_page.append(&files_group);

    let steps_group = PreferencesGroup::builder().title("Install Will").build();
    let step_row = ActionRow::builder()
        .title("dpkg install, then generate and VERIFY the initramfs, then update the boot loader")
        .subtitle("The install fails loudly if initrd.img or the boot menu entry does not appear — no silent unbootable kernels")
        .build();
    steps_group.add(&step_row);
    install_page.append(&steps_group);

    let progress = ProgressBar::new();
    progress.set_show_text(true);
    progress.set_text(Some(""));
    progress.set_visible(false);
    install_page.append(&progress);

    let btn_box = GtkBox::new(Orientation::Horizontal, 8);
    btn_box.set_halign(Align::Center);
    btn_box.set_margin_top(8);

    let install_btn = Button::builder().label("Install Kernel").sensitive(false).build();
    install_btn.add_css_class("pill");
    install_btn.add_css_class("suggested-action");

    let cancel_dl_btn = Button::builder().label("Cancel Download").visible(false).build();
    cancel_dl_btn.add_css_class("pill");
    cancel_dl_btn.add_css_class("destructive-action");

    btn_box.append(&install_btn);
    btn_box.append(&cancel_dl_btn);
    install_page.append(&btn_box);

    let install_scroll = ScrolledWindow::builder().vexpand(true).child(&install_page).build();
    stack.add_titled(&install_scroll, Some("install"), "Install");

    // ═════════════════════════════════════════════════════════════════════════
    //  PAGE 4: Log
    // ═════════════════════════════════════════════════════════════════════════
    let log_page = GtkBox::new(Orientation::Vertical, 0);
    let log_view = TextView::builder()
        .editable(false)
        .monospace(true)
        .wrap_mode(WrapMode::Word)
        .vexpand(true)
        .build();
    log_view.add_css_class("card");
    log_view.set_margin_top(8);
    log_view.set_margin_bottom(8);
    log_view.set_margin_start(8);
    log_view.set_margin_end(8);
    let log_scroll = ScrolledWindow::builder().vexpand(true).child(&log_view).build();
    let clear_btn = Button::builder()
        .label("Clear Log")
        .halign(Align::End)
        .margin_end(8)
        .margin_bottom(8)
        .build();
    clear_btn.add_css_class("flat");
    log_page.append(&log_scroll);
    log_page.append(&clear_btn);
    stack.add_titled(&log_page, Some("log"), "Log");

    // ── Assemble window ───────────────────────────────────────────────────────
    let content = GtkBox::new(Orientation::Vertical, 0);
    content.append(&header);

    let health_banner = Banner::new("A kernel in /boot is missing its initramfs — do not reboot into it. See System tab.");
    health_banner.set_revealed(false);
    content.append(&health_banner);

    content.append(&stack);
    toast_overlay.set_child(Some(&content));
    window.set_content(Some(&toast_overlay));

    // ─────────────────────────────────────────────────────────────────────────
    //  Helpers
    // ─────────────────────────────────────────────────────────────────────────

    let log_fn = {
        let log_view = log_view.clone();
        move |msg: String| {
            let buf = log_view.buffer();
            let mut end = buf.end_iter();
            buf.insert(&mut end, &format!("{}\n", msg));
            let mark = buf.create_mark(None, &buf.end_iter(), false);
            log_view.scroll_to_mark(&mark, 0.0, false, 0.0, 0.0);
        }
    };

    let update_install_tab = {
        let staged_row = staged_row.clone();
        let status_row = status_row.clone();
        let files_list = files_list.clone();
        let install_btn = install_btn.clone();
        let state = state.clone();
        move || {
            let s = state.borrow();
            while let Some(c) = files_list.first_child() { files_list.remove(&c); }

            match (&s.staged_dir, &s.staged_version) {
                (Some(dir), Some(ver)) => {
                    staged_row.set_title(&format!("Linux {}", ver));
                    staged_row.set_subtitle(&dir.display().to_string());

                    match compare_to_running(&s.sysinfo.running_kernel, ver) {
                        VersionRelation::Newer => status_row.set_subtitle(
                            &format!("Upgrade: {} → {}", s.sysinfo.running_kernel, ver)),
                        VersionRelation::Same => status_row.set_subtitle(
                            &format!("Same numeric version as running ({})", s.sysinfo.running_kernel)),
                        VersionRelation::Older => status_row.set_subtitle(
                            &format!("Downgrade: {} → {}", s.sysinfo.running_kernel, ver)),
                        VersionRelation::Unknown => status_row.set_subtitle("—"),
                    }

                    for (fname, verified) in &s.staged_files {
                        let row = ActionRow::builder().title(fname).build();
                        let badge = Label::new(Some(if *verified { "SHA256 ✓" } else { "no checksum" }));
                        badge.add_css_class(if *verified { "success" } else { "dim-label" });
                        row.add_suffix(&badge);
                        files_list.append(&row);
                    }
                    install_btn.set_sensitive(true);
                }
                _ => {
                    staged_row.set_title("Nothing staged");
                    staged_row.set_subtitle("Go to Browse to download a kernel");
                    status_row.set_subtitle("—");
                    install_btn.set_sensitive(false);
                }
            }
        }
    };

    // load_sysinfo — declared as Rc so remove/install callbacks can re-run it
    let load_sysinfo: Rc<dyn Fn()> = {
        let running_row = running_row.clone();
        let kernels_list = kernels_list.clone();
        let boot_row = boot_row.clone();
        let root_row = root_row.clone();
        let health_banner = health_banner.clone();
        let update_banner = update_banner.clone();
        let state = state.clone();
        let update_install_tab = update_install_tab.clone();
        let log_fn = log_fn.clone();
        let window = window.clone();
        let toast_overlay = toast_overlay.clone();

        // Forward declaration hack: the remove action needs load_sysinfo,
        // which is what we're building. A Rc<RefCell<Option<…>>> slot breaks
        // the cycle: the closure looks the callable up at click time.
        let self_slot: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
        let self_slot_outer = self_slot.clone();

        let f: Rc<dyn Fn()> = Rc::new(move || {
            running_row.set_title("Detecting…");
            log_fn("Querying installed kernels…".to_string());

            let running_row = running_row.clone();
            let kernels_list = kernels_list.clone();
            let boot_row = boot_row.clone();
            let root_row = root_row.clone();
            let health_banner = health_banner.clone();
            let update_banner = update_banner.clone();
            let state = state.clone();
            let update_install_tab = update_install_tab.clone();
            let log_fn = log_fn.clone();
            let window = window.clone();
            let toast_overlay = toast_overlay.clone();
            let self_slot = self_slot.clone();

            spawn_async(
                async move { tokio::task::spawn_blocking(query_system).await.unwrap_or_default() },
                move |info: SystemInfo| {
                    running_row.set_title(&info.running_kernel);
                    running_row.set_subtitle("uname -r");

                    // Disk rows
                    match info.free_boot_bytes {
                        Some(f) if f < MIN_BOOT_BYTES => {
                            boot_row.set_subtitle(&format!("Low: {} free", format_bytes(f)));
                        }
                        Some(f) => boot_row.set_subtitle(&format!("{} free", format_bytes(f))),
                        None => boot_row.set_subtitle("Could not determine"),
                    }
                    match info.free_root_bytes {
                        Some(f) if f < MIN_ROOT_BYTES => {
                            root_row.set_subtitle(&format!("Low: {} free (2 GB recommended)", format_bytes(f)));
                        }
                        Some(f) => root_row.set_subtitle(&format!("{} free", format_bytes(f))),
                        None => root_row.set_subtitle("Could not determine"),
                    }

                    // Kernel rows
                    while let Some(c) = kernels_list.first_child() { kernels_list.remove(&c); }
                    let mut any_unhealthy = false;

                    for k in &info.kernels {
                        let mut subtitle_parts = vec![];
                        subtitle_parts.push(if k.has_initrd { "initrd ✓".to_string() }
                                            else { "INITRD MISSING".to_string() });
                        subtitle_parts.push(if k.has_modules { "modules ✓".to_string() }
                                            else { "modules missing".to_string() });
                        subtitle_parts.push(if k.has_boot_entry { "boot menu ✓".to_string() }
                                            else { "NOT IN BOOT MENU".to_string() });

                        let row = ActionRow::builder()
                            .title(&k.version)
                            .subtitle(&subtitle_parts.join(" · "))
                            .build();

                        if k.running {
                            let badge = Label::new(Some("running"));
                            badge.add_css_class("success");
                            row.add_suffix(&badge);
                        } else {
                            let remove_btn = Button::builder()
                                .label("Remove")
                                .valign(Align::Center)
                                .build();
                            remove_btn.add_css_class("destructive-action");
                            remove_btn.add_css_class("flat");

                            let ver = k.version.clone();
                            let window = window.clone();
                            let toast_overlay = toast_overlay.clone();
                            let log_fn = log_fn.clone();
                            let self_slot = self_slot.clone();

                            remove_btn.connect_clicked(move |_| {
                                let dialog = AlertDialog::builder()
                                    .heading(&format!("Remove kernel {}?", ver))
                                    .body("Its packages will be purged and the boot loader updated. The running kernel is never touched.")
                                    .build();
                                dialog.add_responses(&[("cancel", "Cancel"), ("remove", "Remove")]);
                                dialog.set_response_appearance("remove", libadwaita::ResponseAppearance::Destructive);
                                dialog.set_default_response(Some("cancel"));

                                let ver2 = ver.clone();
                                let toast_overlay = toast_overlay.clone();
                                let log_fn = log_fn.clone();
                                let self_slot = self_slot.clone();
                                dialog.connect_response(None, move |_, resp| {
                                    if resp != "remove" { return; }
                                    let ver3 = ver2.clone();
                                    log_fn(format!("Removing kernel {}…", ver3));
                                    let toast_overlay = toast_overlay.clone();
                                    let log_fn = log_fn.clone();
                                    let self_slot = self_slot.clone();
                                    spawn_async(
                                        async move {
                                            tokio::task::spawn_blocking(move || run_privileged_remove(&ver3)).await
                                        },
                                        move |result| {
                                            match result {
                                                Ok(Ok(())) => {
                                                    log_fn("Kernel removed.".to_string());
                                                    toast_overlay.add_toast(Toast::new("Kernel removed"));
                                                }
                                                Ok(Err(e)) => {
                                                    log_fn(format!("Remove failed: {}", e));
                                                    toast_overlay.add_toast(Toast::new("Remove failed — see Log tab"));
                                                }
                                                Err(e) => log_fn(format!("Task error: {}", e)),
                                            }
                                            if let Some(reload) = self_slot.borrow().clone() {
                                                reload();
                                            }
                                        },
                                    );
                                });
                                dialog.present(Some(&window));
                            });

                            row.add_suffix(&remove_btn);
                        }

                        if !k.healthy() {
                            row.add_css_class("error");
                            any_unhealthy = true;
                        }
                        kernels_list.append(&row);
                    }

                    if info.kernels.is_empty() {
                        let row = ActionRow::builder().title("No kernels found in /boot").build();
                        kernels_list.append(&row);
                    }

                    health_banner.set_revealed(any_unhealthy);
                    if any_unhealthy {
                        log_fn("WARNING: at least one installed kernel is missing its initramfs or boot menu entry".to_string());
                    }

                    log_fn(format!(
                        "Running kernel: {} · {} kernels installed",
                        info.running_kernel, info.kernels.len()
                    ));
                    let running_kernel = info.running_kernel.clone();
                    state.borrow_mut().sysinfo = info;
                    update_install_tab();

                    // Nice-to-have, not safety-critical: check for a newer
                    // mainline kernel over the network without blocking the
                    // System tab's (local, fast) render above. A failed
                    // fetch is silent — same Browse-tab data source, just
                    // fire-and-forget here.
                    update_banner.set_revealed(false);
                    spawn_async(fetch_versions(), move |result| {
                        let versions = match result {
                            Ok(v) => v,
                            Err(_e) => {
                                #[cfg(debug_assertions)]
                                eprintln!("update-check: fetch_versions failed: {_e}");
                                return;
                            }
                        };
                        let newest_stable = versions.iter().find(|v| !v.is_rc);
                        if let Some(newest) = newest_stable {
                            if compare_to_running(&running_kernel, &newest.version) == VersionRelation::Newer {
                                update_banner.set_title(&format!(
                                    "Kernel {} is available — see Browse tab to install",
                                    newest.version
                                ));
                                update_banner.set_revealed(true);
                            }
                        }
                    });
                },
            );
        });
        *self_slot_outer.borrow_mut() = Some(f.clone());
        f
    };

    load_sysinfo();

    {
        let ls = load_sysinfo.clone();
        refresh_sysinfo_btn.connect_clicked(move |_| ls());
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Populate browse list — badges relative to the running kernel
    // ─────────────────────────────────────────────────────────────────────────
    fn populate_list(
        list_box: &ListBox,
        versions: &[KernelVersion],
        filter: &str,
        running: &str,
        show_rc: bool,
    ) {
        while let Some(c) = list_box.first_child() { list_box.remove(&c); }
        let filter = filter.to_lowercase();

        for ver in versions {
            if ver.is_rc && !show_rc { continue; }
            if !filter.is_empty() && !ver.version.contains(&filter) { continue; }

            let subtitle = match compare_to_running(running, &ver.version) {
                VersionRelation::Newer  => "Newer than running".to_string(),
                VersionRelation::Same   => "Matches running kernel".to_string(),
                VersionRelation::Older  => "Older than running".to_string(),
                VersionRelation::Unknown => String::new(),
            };

            let row = ActionRow::builder()
                .title(&ver.version)
                .subtitle(&subtitle)
                .activatable(true)
                .build();

            if compare_to_running(running, &ver.version) == VersionRelation::Same {
                row.add_css_class("success");
            }

            if ver.is_rc {
                let badge = Label::new(Some("RC"));
                badge.add_css_class("caption");
                badge.add_css_class("pill");
                badge.add_css_class("warning");
                badge.set_valign(Align::Center);
                row.add_suffix(&badge);
            }

            list_box.append(&row);
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Load versions
    // ─────────────────────────────────────────────────────────────────────────
    let load_versions = {
        let list_box = list_box.clone();
        let list_spinner = list_spinner.clone();
        let selected_label = selected_label.clone();
        let download_btn = download_btn.clone();
        let banner = banner.clone();
        let state = state.clone();
        let log_fn = log_fn.clone();
        let search_entry = search_entry.clone();

        move || {
            list_spinner.start();
            list_spinner.set_visible(true);
            banner.set_revealed(false);
            while let Some(c) = list_box.first_child() { list_box.remove(&c); }
            download_btn.set_sensitive(false);
            selected_label.set_label("Loading…");
            log_fn("Fetching version list from kernel.ubuntu.com…".to_string());

            let list_box = list_box.clone();
            let list_spinner = list_spinner.clone();
            let selected_label = selected_label.clone();
            let banner = banner.clone();
            let state = state.clone();
            let log_fn = log_fn.clone();
            let search_entry = search_entry.clone();

            spawn_async(fetch_versions(), move |result| {
                list_spinner.stop();
                list_spinner.set_visible(false);
                match result {
                    Err(e) => {
                        log_fn(format!("Error fetching versions: {}", e));
                        banner.set_title(&format!("Failed to load versions: {}", e));
                        banner.set_revealed(true);
                        selected_label.set_label("Could not load version list");
                    }
                    Ok(versions) => {
                        let rc_count = versions.iter().filter(|v| v.is_rc).count();
                        log_fn(format!(
                            "Found {} stable versions, {} release candidates",
                            versions.len() - rc_count,
                            rc_count
                        ));
                        state.borrow_mut().versions = versions.clone();
                        let (running, show_rc) = {
                            let s = state.borrow();
                            (s.sysinfo.running_kernel.clone(), s.show_rc)
                        };
                        populate_list(&list_box, &versions, &search_entry.text(), &running, show_rc);
                        selected_label.set_label("No version selected");
                    }
                }
            });
        }
    };

    load_versions();

    { let lv = load_versions.clone(); refresh_btn.connect_clicked(move |_| lv()); }

    // ─────────────────────────────────────────────────────────────────────────
    //  Row selection — connected exactly once, looked up by the row's title
    //  (the version string), never by row index: with an active filter the
    //  visible rows are a subset, so an index into the full versions vec
    //  would select the wrong kernel.
    // ─────────────────────────────────────────────────────────────────────────
    {
        let state = state.clone();
        let download_btn = download_btn.clone();
        let selected_label = selected_label.clone();
        list_box.connect_row_selected(move |_, row| {
            let selected = row
                .and_then(|r| r.downcast_ref::<ActionRow>().map(|ar| ar.title().to_string()))
                .and_then(|title| {
                    state.borrow().versions.iter().find(|v| v.version == title).cloned()
                });

            match selected {
                Some(ver) => {
                    selected_label.set_label(&format!("Selected: {}", ver.version));
                    state.borrow_mut().selected_version = Some(ver);
                    download_btn.set_sensitive(true);
                }
                None => {
                    download_btn.set_sensitive(false);
                    selected_label.set_label("No version selected");
                    state.borrow_mut().selected_version = None;
                }
            }
        });
    }

    {
        let list_box = list_box.clone();
        let state = state.clone();
        search_entry.connect_search_changed(move |entry| {
            // Clone out and drop the borrow first: populate_list can remove the
            // selected row, firing row-selected which takes borrow_mut().
            let (versions, running, show_rc) = {
                let s = state.borrow();
                (s.versions.clone(), s.sysinfo.running_kernel.clone(), s.show_rc)
            };
            populate_list(&list_box, &versions, &entry.text(), &running, show_rc);
        });
    }

    {
        let list_box = list_box.clone();
        let state = state.clone();
        let search_entry = search_entry.clone();
        rc_toggle.connect_state_set(move |_, active| {
            state.borrow_mut().show_rc = active;
            let (versions, running) = {
                let s = state.borrow();
                (s.versions.clone(), s.sysinfo.running_kernel.clone())
            };
            populate_list(&list_box, &versions, &search_entry.text(), &running, active);
            glib::Propagation::Proceed
        });
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Download — resolve the deb set, then download + verify each file
    // ─────────────────────────────────────────────────────────────────────────
    {
        let state = state.clone();
        let progress = progress.clone();
        let log_fn = log_fn.clone();
        let update_install_tab = update_install_tab.clone();
        let stack = stack.clone();
        let toast_overlay = toast_overlay.clone();
        let cancel_dl_btn = cancel_dl_btn.clone();

        download_btn.connect_clicked(move |btn| {
            let ver = { state.borrow().selected_version.clone() };
            let Some(ver) = ver else { return };

            if let Some(f) = state.borrow().sysinfo.free_root_bytes {
                if f < MIN_ROOT_BYTES {
                    toast_overlay.add_toast(Toast::new("Low disk space — install may fail"));
                }
            }

            btn.set_sensitive(false);
            cancel_dl_btn.set_visible(true);
            progress.set_visible(true);
            progress.set_fraction(0.0);
            progress.set_text(Some(&format!("Resolving packages for {}…", ver.version)));
            stack.set_visible_child_name("install");
            log_fn(format!("Resolving package list for Linux {}…", ver.version));

            let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
            state.borrow_mut().download_cancel = Some(cancel_flag.clone());

            let dest_dir = PathBuf::from(format!(
                "{}/Downloads/mainline-kernel-v{}",
                std::env::var("HOME").unwrap_or("/tmp".into()),
                ver.version
            ));

            // Progress channel — sent from the Tokio side, drained on the GTK
            // main context; the loop exits when the sender drops.
            let (prog_tx, mut prog_rx) = tokio::sync::mpsc::unbounded_channel::<Progress>();
            let download_start = std::time::Instant::now();

            {
                let progress = progress.clone();
                glib::MainContext::default().spawn_local(async move {
                    let mut last_ui = std::time::Instant::now();
                    while let Some(mut msg) = prog_rx.recv().await {
                        while let Ok(newer) = prog_rx.try_recv() { msg = newer; }
                        if last_ui.elapsed() < std::time::Duration::from_millis(100) {
                            continue;
                        }
                        last_ui = std::time::Instant::now();
                        let label_prefix = format!(
                            "[{}/{}] {}",
                            msg.file_index + 1, msg.file_count, msg.filename
                        );
                        if let Some(t) = msg.total {
                            if t > 0 {
                                progress.set_fraction(msg.downloaded as f64 / t as f64);
                            }
                            let elapsed = download_start.elapsed().as_secs_f64();
                            let speed = if elapsed > 0.0 { msg.downloaded as f64 / elapsed } else { 0.0 };
                            progress.set_text(Some(&format!(
                                "{}  {:.1} / {:.1} MB ({:.1} MB/s)",
                                label_prefix,
                                msg.downloaded as f64 / 1_000_000.0,
                                t as f64 / 1_000_000.0,
                                speed / 1_000_000.0,
                            )));
                        } else {
                            progress.set_text(Some(&label_prefix));
                            progress.pulse();
                        }
                    }
                });
            }

            let state = state.clone();
            let progress = progress.clone();
            let log_fn = log_fn.clone();
            let update_install_tab = update_install_tab.clone();
            let btn = btn.clone();
            let toast_overlay = toast_overlay.clone();
            let cancel_dl_btn = cancel_dl_btn.clone();
            let cancel_flag2 = cancel_flag.clone();
            let ver2 = ver.clone();
            let dest_dir2 = dest_dir.clone();

            spawn_async(
                async move {
                    let debs = fetch_deb_list(&ver2).await?;
                    let file_info: Vec<(String, bool)> = debs
                        .iter()
                        .map(|d| (d.filename.clone(), d.sha256.is_some()))
                        .collect();
                    let tx = prog_tx;
                    download_deb_set(
                        debs,
                        dest_dir2,
                        move |p| { let _ = tx.send(p); },
                        cancel_flag2,
                    )
                    .await?;
                    Ok::<Vec<(String, bool)>, anyhow::Error>(file_info)
                },
                move |result| {
                    btn.set_sensitive(true);
                    cancel_dl_btn.set_visible(false);
                    state.borrow_mut().download_cancel = None;

                    match result {
                        Err(e) => {
                            let msg = e.to_string();
                            if msg.contains("cancelled") {
                                log_fn("Download cancelled.".to_string());
                                progress.set_text(Some("Cancelled"));
                            } else {
                                log_fn(format!("Download failed: {:#}", e));
                                progress.set_text(Some("Download failed"));
                                toast_overlay.add_toast(Toast::new("Download failed — see Log tab"));
                            }
                        }
                        Ok(file_info) => {
                            let verified = file_info.iter().filter(|(_, v)| *v).count();
                            log_fn(format!(
                                "Downloaded {} packages ({} SHA256-verified) to {}",
                                file_info.len(), verified, dest_dir.display()
                            ));
                            progress.set_fraction(1.0);
                            progress.set_text(Some("Ready to install"));
                            {
                                let mut s = state.borrow_mut();
                                s.staged_dir = Some(dest_dir.clone());
                                s.staged_version = Some(ver.version.clone());
                                s.staged_files = file_info;
                            }
                            update_install_tab();
                            toast_overlay.add_toast(Toast::new("Download complete"));
                        }
                    }
                },
            );
        });
    }

    // Cancel download
    {
        let state = state.clone();
        cancel_dl_btn.connect_clicked(move |_| {
            if let Some(flag) = &state.borrow().download_cancel {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        });
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Install
    // ─────────────────────────────────────────────────────────────────────────
    {
        let state = state.clone();
        let log_fn = log_fn.clone();
        let stack = stack.clone();
        let toast_overlay = toast_overlay.clone();
        let progress = progress.clone();
        let load_sysinfo = load_sysinfo.clone();

        install_btn.connect_clicked(move |btn| {
            let dir = { state.borrow().staged_dir.clone() };
            let Some(dir) = dir else { return };
            let dir_str = dir.display().to_string();

            btn.set_sensitive(false);
            progress.set_visible(true);
            progress.set_text(Some("Installing… initramfs is generated and verified before success"));
            stack.set_visible_child_name("log");
            log_fn(format!("Starting kernel install from {}", dir_str));

            {
                let progress = progress.clone();
                let btn = btn.clone();
                glib::timeout_add_local(std::time::Duration::from_millis(150), move || {
                    if btn.is_sensitive() {
                        glib::ControlFlow::Break
                    } else {
                        progress.pulse();
                        glib::ControlFlow::Continue
                    }
                });
            }

            let log_fn = log_fn.clone();
            let toast_overlay = toast_overlay.clone();
            let progress = progress.clone();
            let btn = btn.clone();
            let load_sysinfo = load_sysinfo.clone();

            spawn_async(
                async move {
                    tokio::task::spawn_blocking(move || run_privileged_install(&dir_str)).await
                },
                move |result| {
                    btn.set_sensitive(true);
                    progress.set_visible(false);
                    match result {
                        Ok(Ok(())) => {
                            log_fn("Install completed — initramfs verified in /boot.".to_string());
                            log_fn("Reboot to switch to the new kernel.".to_string());
                            toast_overlay.add_toast(Toast::new("Kernel installed — reboot to activate"));
                            load_sysinfo();
                        }
                        Ok(Err(e)) => {
                            log_fn(format!("Install failed: {}", e));
                            toast_overlay.add_toast(Toast::new("Install failed — see Log tab"));
                        }
                        Err(e) => log_fn(format!("Task error: {}", e)),
                    }
                },
            );
        });
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  About dialog
    // ─────────────────────────────────────────────────────────────────────────
    {
        let window = window.clone();
        let state = state.clone();
        about_btn.connect_clicked(move |_| {
            let (running, count) = {
                let s = state.borrow();
                (s.sysinfo.running_kernel.clone(), s.sysinfo.kernels.len())
            };
            let dialog = AboutDialog::builder()
                .application_name("KernelPop")
                .version(env!("CARGO_PKG_VERSION"))
                .developers(vec!["Linnard Alex Brown Jr."])
                .comments(format!(
                    "GTK4 + Rust GUI for installing Ubuntu mainline kernels with a verified initramfs.\n\nRunning kernel: {}\nInstalled kernels: {}",
                    running, count
                ))
                .build();
            dialog.add_acknowledgement_section(Some("Built with"), &["Claude Code (Anthropic)"]);
            dialog.present(Some(&window));
        });
    }

    // Clear log
    { let lv = log_view.clone(); clear_btn.connect_clicked(move |_| { lv.buffer().set_text(""); }); }

    window.present();
}
