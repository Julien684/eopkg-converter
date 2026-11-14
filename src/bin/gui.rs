//! `eopkg-converter-gui` — interface graphique Qt6 (qtrs) du convertisseur.
//!
//! Fenêtre unique : on ajoute des paquets `.deb` / `.rpm` (fichier ou dossier),
//! on choisit le dossier de destination, on lance la conversion. Un thread
//! effectue le travail et alimente la barre de progression, le statut et le
//! journal via un canal de messages pompé par un `QTimer` (le même schéma
//! qu'`amv-converter`) pour que l'interface reste réactive pendant la
//! conversion.
//!
//! Modèle qtrs : les widgets sont posés dans l'arborescence Qt (ils lui
//! appartiennent) et portent un objet-nom ; les callbacks les repèrent via
//! `QWidget_findX(central, name)` (les `widget_ptr()` des wrappers étant
//! `pub(crate)`). L'état source de vérité (liste de fichiers) est un
//! `Rc<RefCell<Vec<String>>>` partagé ; la barre de progression, le statut, le
//! journal, le bouton et le champ de destination sont mis à jour par pointeur
//! FFI. Les callbacks capturent des `Rc<..>` / `Arc<..>` (jamais de cycle).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use cxx::let_cxx_string;
use qtrs::ffi;
use qtrs::prelude::*;
use qtrs::{
    Application, CheckBox, FileDialog, Frame, HBoxLayout, Label, LineEdit, ListWidget, MainWindow,
    Menu, MenuBar, MessageBox, PlainTextEdit, ProgressBar, PushButton, Timer, VBoxLayout, Widget,
};

use eopkg_converter::{convert, i18n, rpm, TOOL_NAME, VERSION};

/// Messages envoyés par le thread de travail vers l'interface.
enum Msg {
    /// Ligne pour le journal.
    Log(String),
    /// Barre de progression (index du paquet courant sur le total).
    Progress { index: usize, total: usize },
    /// Fin : bilan (succès, échec).
    Done { ok: usize, fail: usize },
}

/// Chemin de l'icône de l'application (PNG hicolor, si présente).
fn resolve_icon() -> Option<String> {
    if let Some(exe) = std::env::current_exe().ok() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join("eopkg-converter.png");
            if cand.is_file() {
                return Some(cand.to_string_lossy().into_owned());
            }
        }
    }
    for name in ["assets/eopkg-converter-128.png", "assets/eopkg-converter.png"] {
        let p = std::path::Path::new(name);
        if p.is_file() {
            return Some(p.to_string_lossy().into_owned());
        }
    }
    None
}

/// Un chemin est convertible s'il porte l'extension `.deb` ou `.rpm`.
fn is_supported(p: &str) -> bool {
    let low = p.to_lowercase();
    low.ends_with(".deb") || low.ends_with(".rpm")
}

/// Installe un `.eopkg` produit, via `pkexec` (élévation polkit).
///
/// Renvoie une ligne de journal décrivant le résultat. L'installation exige
/// les droits root : sans `pkexec`, on le signale sans échouer la conversion.
fn install_package(path: &std::path::Path) -> String {
    use std::process::Command;
    let status = Command::new("pkexec")
        .arg("eopkg")
        .arg("install")
        .arg("-y")
        .arg(path)
        .status();
    match status {
        Ok(s) if s.success() => format!("Installé : {}", path.display()),
        Ok(s) => format!(
            "Installation refusée/échouée (code {}) : {}",
            s.code().unwrap_or(-1),
            path.display()
        ),
        Err(e) => format!(
            "Impossible de lancer pkexec ({e}) — installez manuellement {}",
            path.display()
        ),
    }
}

/// Convertit la liste de paquets dans `out_dir` (thread de travail).
fn convert_all(files: Vec<String>, out_dir: String, install_after: bool, tx: Sender<Msg>) {
    let lang = i18n::Lang::detect();
    // Le journal de la GUI reçoit toutes les lignes de la bibliothèque : on
    // branche le sink sur le même canal que les messages du thread.
    let sink_tx = std::sync::Arc::new(Mutex::new(tx.clone()));
    let sink: convert::LogSink = std::sync::Arc::new(move |line: &str| {
        if let Ok(tx) = sink_tx.lock() {
            let _ = tx.send(Msg::Log(line.to_string()));
        }
    });
    let opts = convert::Options {
        keep_deps: false,
        quiet: true,
        lang,
        log_sink: Some(sink),
    };
    let out_path = std::path::PathBuf::from(&out_dir);
    let mut ok = 0usize;
    let mut fail = 0usize;
    let total = files.len();

    for (i, file) in files.iter().enumerate() {
        let _ = tx.send(Msg::Log(format!("({}/{}) {}", i + 1, total, file)));
        let _ = tx.send(Msg::Progress { index: i, total });
        let is_rpm = file.to_lowercase().ends_with(".rpm");
        let res = if is_rpm {
            rpm::convert(std::path::Path::new(file), &out_path, opts.clone())
        } else {
            convert::convert(std::path::Path::new(file), &out_path, opts.clone())
        };
        match res {
            Ok(summary) => {
                ok += 1;
                let mut line = format!(
                    "OK → {} ({} fichiers, {})",
                    summary.out.display(),
                    summary.files,
                    convert::human_size(summary.bytes, lang)
                );
                if let Some(dir) = &summary.scripts_dir {
                    line.push_str(&format!("\n     dossier métadonnées : {}", dir.display()));
                }
                let _ = tx.send(Msg::Log(line));
                if !summary.deps.is_empty() {
                    let _ = tx.send(Msg::Log(format!(
                        "     {} dépendance(s) déclarée(s) :",
                        summary.deps.len()
                    )));
                    for d in &summary.deps {
                        let c = d
                            .constraint
                            .as_ref()
                            .map(|c| format!(" ({c})"))
                            .unwrap_or_default();
                        let _ = tx.send(Msg::Log(format!("       • {}{c}", d.name)));
                    }
                }
                if !summary.scripts.is_empty() {
                    let names: Vec<&str> = summary
                        .scripts
                        .iter()
                        .map(|s| convert::script_file_name(&s.name))
                        .collect();
                    let _ = tx.send(Msg::Log(format!(
                        "     scripts exportés : {}",
                        names.join(", ")
                    )));
                }
                if install_after {
                    let msg = install_package(&summary.out);
                    let _ = tx.send(Msg::Log(format!("     {msg}")));
                }
            }
            Err(err) => {
                fail += 1;
                let _ = tx.send(Msg::Log(format!("ÉCHEC ({}) → {err}", file)));
            }
        }
    }
    let _ = tx.send(Msg::Progress {
        index: total,
        total,
    });
    let _ = tx.send(Msg::Done { ok, fail });
}

/// Ajoute des chemins convertibles à l'état source de vérité (sans doublons).
fn push_paths(files: &Rc<RefCell<Vec<String>>>, paths: &[String]) {
    let mut f = files.borrow_mut();
    for p in paths {
        if is_supported(p) && !f.contains(p) {
            f.push(p.clone());
        }
    }
}

/// Synchronise la liste visuelle avec l'état (on la vide puis on réécrit).
fn sync_list(central: *mut ffi::QWidget, files: &Rc<RefCell<Vec<String>>>) {
    unsafe {
        let_cxx_string!(n = "filelist");
        let lw = ffi::QWidget_findListWidget(central, &n);
        if lw.is_null() {
            return;
        }
        ffi::QListWidget_clear(lw);
        for p in files.borrow().iter() {
            let_cxx_string!(s = p.as_str());
            ffi::QListWidget_addItem(lw, &s);
        }
    }
}

/// Lit le texte d'un `QLineEdit` identifié par son objet-nom.
fn read_lineedit(central: *mut ffi::QWidget, name: &str) -> Option<String> {
    unsafe {
        let_cxx_string!(n = name);
        let le = ffi::QWidget_findLineEdit(central, &n);
        if le.is_null() {
            None
        } else {
            Some(ffi::QLineEdit_text(le))
        }
    }
}

/// Met à jour un libellé repéré par son objet-nom.
fn set_label(central: *mut ffi::QWidget, name: &str, text: &str) {
    unsafe {
        let_cxx_string!(n = name);
        let lb = ffi::QWidget_findLabel(central, &n);
        if lb.is_null() {
            return;
        }
        let_cxx_string!(s = text);
        ffi::QLabel_setText(lb, &s);
    }
}

/// Ajoute une ligne au journal (`QPlainTextEdit` nommé « log »).
fn append_log(central: *mut ffi::QWidget, text: &str) {
    unsafe {
        let_cxx_string!(n = "log");
        let w = ffi::QWidget_findPlainTextEdit(central, &n);
        if w.is_null() {
            return;
        }
        let_cxx_string!(s = text);
        ffi::QPlainTextEdit_appendPlainText(w, &s);
    }
}

/// Lit l'état d'une case à cocher repérée par son objet-nom.
fn is_checked(central: *mut ffi::QWidget, name: &str) -> bool {
    unsafe {
        let_cxx_string!(n = name);
        let cb = ffi::QWidget_findCheckBox(central, &n);
        if cb.is_null() {
            false
        } else {
            ffi::QCheckBox_isChecked(cb)
        }
    }
}

/// Démarre un `QTimer` qui pompe le canal de messages vers l'interface.
///
/// Le wrapper `Timer` est « oublié » volontairement : son `Drop` supprime le
/// `QTimer` C++, et nous voulons qu'il vive toute la session.
fn start_pump(rx: Receiver<Msg>, central: *mut ffi::QWidget, running: Arc<Mutex<bool>>) {
    let rx = Arc::new(Mutex::new(rx));
    let timer = Timer::new(100)
        .on_timeout({
            let rx = Arc::clone(&rx);
            let running = Arc::clone(&running);
            move || {
                let rx = rx.lock().unwrap();
                let mut got_done = false;
                let mut summary = String::new();
                while let Ok(m) = rx.try_recv() {
                    match m {
                        Msg::Log(line) => append_log(central, &line),
                        Msg::Progress { index, total } => unsafe {
                            let_cxx_string!(n = "bar");
                            let bar = ffi::QWidget_findProgressBar(central, &n);
                            if !bar.is_null() && total > 0 {
                                let v = (index * 100 / total).min(100) as i32;
                                ffi::QProgressBar_setValue(bar, v);
                            }
                        },
                        Msg::Done { ok, fail } => {
                            got_done = true;
                            summary = format!("Terminé : {ok} réussi(s), {fail} échec(s).");
                        }
                    }
                }
                if got_done {
                    unsafe {
                        let_cxx_string!(n = "go");
                        let go = ffi::QWidget_findPushButton(central, &n);
                        if !go.is_null() {
                            ffi::QWidget_setEnabled(to_qwidget(go), true);
                        }
                        let_cxx_string!(n2 = "bar");
                        let bar = ffi::QWidget_findProgressBar(central, &n2);
                        if !bar.is_null() {
                            ffi::QProgressBar_setValue(bar, 100);
                        }
                    }
                    set_label(central, "status", &summary);
                    append_log(central, &summary);
                    *running.lock().unwrap() = false;
                }
            }
        })
        .build();
    #[allow(clippy::mem_forget)]
    std::mem::forget(timer);
}

/// Convertit un pointeur typé en `*mut QWidget` (cast générique).
fn to_qwidget<T>(p: *mut T) -> *mut ffi::QWidget {
    p as *mut ffi::QWidget
}

/// Texte de la boîte « À propos ».
fn about_text() -> String {
    format!(
        "{TOOL_NAME} {VERSION}\n\n\
Convertit un paquet Debian (.deb) ou un paquet RPM (.rpm) en paquet\n\
Solus (.eopkg) installable.\n\n\
À chaque conversion, un dossier portant le nom du paquet est créé à côté\n\
du .eopkg. Il contient :\n\
  • pre-install    — script de pré-installation du paquet d'origine\n\
  • post-install   — script de post-installation\n\
  • pre-remove     — script de pré-suppression\n\
  • post-remove    — script de post-suppression\n\
  • dependencies.txt — dépendances déclarées par le paquet d'origine\n\n\
eopkg n'exécute pas ces scripts : ce dossier documente ce que le paquet\n\
source prévoyait, et les dépendances qu'il déclarait avant conversion."
    )
}

fn main() {
    let app = Application::new();
    if let Some(icon) = resolve_icon() {
        app.set_icon(&icon);
    }

    let window = MainWindow::new()
        .window_title(format!("{TOOL_NAME} {VERSION}"))
        .size(820, 620)
        .build();

    // ---- barre de menus ----
    let file_menu = Menu::new("Fichier")
        .action("Quitter", || std::process::exit(0))
        .build();
    let help_menu = Menu::new("Aide")
        .action("À propos", || {
            MessageBox::about(None, &format!("À propos de {TOOL_NAME}"), &about_text());
        })
        .build();
    let menubar = MenuBar::new()
        .add_menu(file_menu)
        .add_menu(help_menu)
        .build();
    window.set_menu_bar(&menubar);
    // `set_menu_bar` confie la barre à la fenêtre Qt : on oublie le wrapper
    // pour éviter un double `delete` à la fermeture (même logique que Timer).
    std::mem::forget(menubar);

    let central = Widget::new().build();
    let mut root = VBoxLayout::with_parent(&central);
    let central_ptr = central.widget_ptr();

    // État source de vérité partagé avec les callbacks (thread UI).
    let files: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let running = Arc::new(Mutex::new(false));

    // ---- barre d'outils ----
    let topbar = Frame::new().parent(&central).build();
    let mut top_h = HBoxLayout::with_parent(&topbar);
    let btn_add = PushButton::new("+ Ajouter…")
        .parent(&topbar)
        .on_clicked({
            let files = Rc::clone(&files);
            let central = central_ptr;
            move || {
                let picked = FileDialog::open_files(
                    None,
                    "Choisir des paquets .deb / .rpm",
                    "",
                    "Paquets (*.deb *.rpm);;Tous (*)",
                );
                push_paths(&files, &picked);
                sync_list(central, &files);
            }
        })
        .build();
    let btn_dir = PushButton::new("Dossier…")
        .parent(&topbar)
        .on_clicked({
            let files = Rc::clone(&files);
            let central = central_ptr;
            move || {
                if let Some(dir) = FileDialog::select_directory(None, "Choisir un dossier", "") {
                    if let Ok(rd) = std::fs::read_dir(&dir) {
                        let v: Vec<String> = rd
                            .flatten()
                            .map(|e| e.path().to_string_lossy().into_owned())
                            .filter(|p| is_supported(p))
                            .collect();
                        push_paths(&files, &v);
                        sync_list(central, &files);
                    }
                }
            }
        })
        .build();
    let btn_clear = PushButton::new("Tout retirer")
        .parent(&topbar)
        .on_clicked({
            let files = Rc::clone(&files);
            let central = central_ptr;
            move || {
                files.borrow_mut().clear();
                sync_list(central, &files);
            }
        })
        .build();
    top_h.add(btn_add);
    top_h.add(btn_dir);
    top_h.add(btn_clear);
    root.add(topbar);

    // ---- liste des paquets ----
    let list = ListWidget::new().parent(&central).build();
    list.set_object_name("filelist");
    root.add(list);

    // ---- destination ----
    let destbar = Frame::new().parent(&central).build();
    let mut dest_h = HBoxLayout::with_parent(&destbar);
    let dest_label = Label::new("Destination :").parent(&destbar).build();
    let dest_edit = LineEdit::new(String::from(".")).parent(&destbar).build();
    dest_edit.set_object_name("dest");
    let btn_choose = PushButton::new("Choisir…")
        .parent(&destbar)
        .on_clicked({
            let central = central_ptr;
            move || {
                if let Some(dir) =
                    FileDialog::select_directory(None, "Dossier de destination", "")
                {
                    unsafe {
                        let_cxx_string!(n = "dest");
                        let le = ffi::QWidget_findLineEdit(central, &n);
                        if !le.is_null() {
                            let_cxx_string!(s = dir.as_str());
                            ffi::QLineEdit_setText(le, &s);
                        }
                    }
                }
            }
        })
        .build();
    dest_h.add(dest_label);
    dest_h.add(dest_edit);
    dest_h.add(btn_choose);
    root.add(destbar);

    // ---- case « installer après conversion » ----
    let install_cb = CheckBox::new("Installer le paquet après la conversion")
        .parent(&central)
        .build();
    install_cb.set_object_name("install");
    root.add(install_cb);

    // ---- progression + statut + bouton ----
    let actbar = Frame::new().parent(&central).build();
    let mut act_h = HBoxLayout::with_parent(&actbar);
    let bar = ProgressBar::new().parent(&actbar).build();
    bar.set_object_name("bar");
    bar.set_range(0, 100);
    let status = Label::new("Prêt.").parent(&actbar).build();
    status.set_object_name("status");
    let go = PushButton::new("Convertir")
        .parent(&actbar)
        .on_clicked({
            let files = Rc::clone(&files);
            let running = Arc::clone(&running);
            let central = central_ptr;
            move || {
                let snapshot: Vec<String> = files.borrow().clone();
                if snapshot.is_empty() {
                    set_label(central, "status", "Ajoutez au moins un paquet.");
                    return;
                }
                {
                    let mut r = running.lock().unwrap();
                    if *r {
                        set_label(central, "status", "Conversion déjà en cours…");
                        return;
                    }
                    *r = true;
                }
                let out_dir = read_lineedit(central, "dest").unwrap_or_else(|| ".".into());
                let install_after = is_checked(central, "install");
                unsafe {
                    let_cxx_string!(n = "go");
                    let go = ffi::QWidget_findPushButton(central, &n);
                    if !go.is_null() {
                        ffi::QWidget_setEnabled(to_qwidget(go), false);
                    }
                    let_cxx_string!(n2 = "bar");
                    let bar = ffi::QWidget_findProgressBar(central, &n2);
                    if !bar.is_null() {
                        ffi::QProgressBar_setRange(bar, 0, 100);
                        ffi::QProgressBar_setValue(bar, 0);
                    }
                    // Nouvelle conversion : on repart d'un journal vierge.
                    let_cxx_string!(n3 = "log");
                    let log = ffi::QWidget_findPlainTextEdit(central, &n3);
                    if !log.is_null() {
                        ffi::QPlainTextEdit_clear(log);
                    }
                }
                set_label(central, "status", "Conversion en cours…");
                append_log(central, "Démarrage de la conversion…");
                let (tx, rx) = mpsc::channel::<Msg>();
                std::thread::spawn(move || convert_all(snapshot, out_dir, install_after, tx));
                start_pump(rx, central, Arc::clone(&running));
            }
        })
        .build();
    go.set_object_name("go");
    act_h.add(bar);
    act_h.add(status);
    act_h.add(go);
    root.add(actbar);

    // ---- journal ----
    let log = PlainTextEdit::new().parent(&central).build();
    log.set_object_name("log");
    log.set_placeholder_text("Journal de conversion…");
    root.add(log);

    window.set_central_widget(&central);
    window.show();
    app.exec();
}
