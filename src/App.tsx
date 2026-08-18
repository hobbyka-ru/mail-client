import { FormEvent, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import "./App.css";

type Account = { email: string; imapHost: string; imapPort: number; smtpHost: string; smtpPort: number };
type Folder = { name: string; path: string; rawPath: string; specialUse?: string; total: number; unseen: number };
type Label = { name: string; count: number };
type Message = { uid: number; subject: string; fromName: string; fromAddress: string; date: number; seen: boolean; flagged: boolean; labels: string[]; size: number };
type Page = { items: Message[]; total: number; unseen: number };
type Detail = Message & { to: string; bodyText?: string; bodyHtml?: string };

const defaults: Account = { email: "", imapHost: "imap.yandex.ru", imapPort: 993, smtpHost: "smtp.yandex.ru", smtpPort: 465 };
const glyphs: Record<string, string> = { inbox: "▰", sent: "↗", drafts: "□", trash: "⌫", junk: "!", archive: "▣" };

function friendlyDate(value: number) {
  const date = new Date(value * 1000);
  return date.toDateString() === new Date().toDateString()
    ? date.toLocaleTimeString("ru", { hour: "2-digit", minute: "2-digit" })
    : date.toLocaleDateString("ru", { day: "numeric", month: "short" });
}

export default function App() {
  const [account, setAccount] = useState<Account>();
  const [folders, setFolders] = useState<Folder[]>([]);
  const [labels, setLabels] = useState<Label[]>([]);
  const [folder, setFolder] = useState<Folder>();
  const [page, setPage] = useState<Page>({ items: [], total: 0, unseen: 0 });
  const [selected, setSelected] = useState<Detail>();
  const [activeLabel, setActiveLabel] = useState("");
  const [search, setSearch] = useState("");
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [compose, setCompose] = useState(false);

  useEffect(() => {
    invoke<Account>("get_account").then((value) => { setAccount(value); void boot(); }).catch(() => setLoading(false));
  }, []);

  async function boot() {
    setLoading(true); setError("");
    try {
      const nextFolders = await invoke<Folder[]>("list_folders");
      setFolders(nextFolders);
      const inbox = nextFolders.find((item) => item.specialUse === "inbox") ?? nextFolders[0];
      if (inbox) await openFolder(inbox);
      invoke<Label[]>("list_labels", { rawPath: inbox?.rawPath ?? "INBOX" }).then(setLabels).catch(() => undefined);
    } catch (reason) { setError(String(reason)); }
    finally { setLoading(false); }
  }

  async function loadMessages(target = folder, label = activeLabel, query = search) {
    if (!target) return;
    setLoading(true); setError("");
    try {
      const nextPage = await invoke<Page>("list_messages", { rawPath: target.rawPath, limit: 60, label: label || null, query: query || null });
      setPage(nextPage);
      const visibleLabels = new Map<string, number>();
      nextPage.items.flatMap((item) => item.labels).forEach((name) => visibleLabels.set(name, (visibleLabels.get(name) ?? 0) + 1));
      setLabels((items) => [...items, ...[...visibleLabels].filter(([name]) => !items.some((item) => item.name === name)).map(([name, count]) => ({ name, count }))]);
      if (!label && !query) setFolders((items) => items.map((item) => item.rawPath === target.rawPath ? { ...item, total: nextPage.total, unseen: nextPage.unseen } : item));
    } catch (reason) { setError(String(reason)); }
    finally { setLoading(false); }
  }

  async function openFolder(next: Folder) {
    setFolder(next); setActiveLabel(""); setSelected(undefined);
    await loadMessages(next, "", "");
  }

  async function openLabel(name: string) {
    const inbox = folders.find((item) => item.specialUse === "inbox") ?? folder;
    if (!inbox) return;
    setFolder(inbox); setActiveLabel(name); setSelected(undefined);
    await loadMessages(inbox, name, "");
  }

  async function openMessage(message: Message) {
    if (!folder) return;
    setSelected(await invoke<Detail>("get_message", { rawPath: folder.rawPath, uid: message.uid }));
    if (!message.seen) {
      await invoke("set_flag", { rawPath: folder.rawPath, uid: message.uid, flag: "seen", enabled: true });
      setPage((value) => ({ ...value, items: value.items.map((item) => item.uid === message.uid ? { ...item, seen: true } : item) }));
    }
  }

  async function toggleStar(message: Message) {
    if (!folder) return;
    await invoke("set_flag", { rawPath: folder.rawPath, uid: message.uid, flag: "flagged", enabled: !message.flagged });
    const patch = (item: Message) => item.uid === message.uid ? { ...item, flagged: !message.flagged } : item;
    setPage((value) => ({ ...value, items: value.items.map(patch) }));
    setSelected((value) => value?.uid === message.uid ? { ...value, flagged: !message.flagged } : value);
  }

  async function toggleLabel(name: string) {
    if (!folder || !selected) return;
    const enabled = !selected.labels.includes(name);
    await invoke("set_label", { rawPath: folder.rawPath, uid: selected.uid, label: name, enabled });
    setSelected({ ...selected, labels: enabled ? [...selected.labels, name] : selected.labels.filter((item) => item !== name) });
    setLabels((items) => items.map((item) => item.name === name ? { ...item, count: item.count + (enabled ? 1 : -1) } : item));
  }

  async function moveTo(destination: Folder) {
    if (!folder || !selected) return;
    await invoke("move_message", { rawPath: folder.rawPath, uid: selected.uid, destination: destination.rawPath });
    setSelected(undefined); await loadMessages();
  }

  const title = activeLabel ? `Метка «${activeLabel}»` : folder?.name ?? "Почта";
  const knownLabels = useMemo(() => Array.from(new Set([...labels.map((item) => item.name), ...(selected?.labels ?? [])])), [labels, selected]);

  if (!account) return <AccountScreen onSaved={(value) => { setAccount(value); void boot(); }} />;

  return <div className="app-shell">
    <aside className="sidebar">
      <div className="brand"><span className="brand-mark">Я</span><strong>Почта</strong><span className="account">{account.email}</span></div>
      <button className="compose-button" onClick={() => setCompose(true)}>✎&nbsp;&nbsp;Написать</button>
      <nav className="nav-list" aria-label="Папки">
        {folders.map((item) => <button key={item.rawPath} className={folder?.rawPath === item.rawPath && !activeLabel ? "active" : ""} onClick={() => void openFolder(item)}>
          <span className="nav-glyph">{glyphs[item.specialUse ?? ""] ?? "○"}</span><span>{item.name}</span>{item.unseen > 0 && <b>{item.unseen}</b>}
        </button>)}
      </nav>
      <div className="section-title">Метки на сервере</div>
      <nav className="nav-list labels" aria-label="Метки">
        {labels.map((item, index) => <button key={item.name} className={activeLabel === item.name ? "active" : ""} onClick={() => void openLabel(item.name)}>
          <i className={`label-dot dot-${index % 5}`} /><span>{item.name}</span><b>{item.count}</b>
        </button>)}
        {!labels.length && <p className="empty-labels">Метки появятся после чтения с сервера</p>}
      </nav>
    </aside>

    <main className="mail-area">
      <header className="topbar">
        <form onSubmit={(event) => { event.preventDefault(); void loadMessages(folder, "", search); setActiveLabel(""); }}>
          <span>⌕</span><input value={search} onChange={(event) => setSearch(event.target.value)} placeholder="Найти письмо" aria-label="Найти письмо" />
        </form>
        <button className="round" onClick={() => void loadMessages()} title="Обновить">↻</button>
        <div className="avatar">{account.email[0]?.toUpperCase()}</div>
      </header>
      <div className="mail-columns">
        <section className={`message-column ${selected ? "with-reader" : ""}`}>
          <div className="list-heading"><div><h1>{title}</h1><span>{page.total.toLocaleString("ru")} писем</span></div>{loading && <span className="spinner">Обновляем…</span>}</div>
          {error && <div className="error">{error}<button onClick={() => void loadMessages()}>Повторить</button></div>}
          <div className="message-list">
            {!loading && !page.items.length && <div className="empty">Здесь пока нет писем</div>}
            {page.items.map((message) => <article key={message.uid} className={`${message.seen ? "" : "unread"} ${selected?.uid === message.uid ? "selected" : ""}`} onClick={() => void openMessage(message)}>
              <button className={`star ${message.flagged ? "on" : ""}`} onClick={(event) => { event.stopPropagation(); void toggleStar(message); }} aria-label="Пометить важным">★</button>
              <div className="sender-avatar">{(message.fromName || message.fromAddress || "?")[0].toUpperCase()}</div>
              <div className="message-copy"><div className="message-top"><strong>{message.fromName || message.fromAddress}</strong><time>{friendlyDate(message.date)}</time></div>
                <div className="subject">{message.subject}</div><div className="chips">{message.labels.map((label) => <span key={label}>{label}</span>)}</div>
              </div>
            </article>)}
          </div>
        </section>
        {selected && <section className="reader">
          <div className="reader-actions"><button onClick={() => setSelected(undefined)}>←</button><button className={`star ${selected.flagged ? "on" : ""}`} onClick={() => void toggleStar(selected)}>★</button>
            <select aria-label="Переместить письмо" defaultValue="" onChange={(event) => { const target = folders.find((item) => item.rawPath === event.target.value); if (target) void moveTo(target); }}><option value="" disabled>Переместить…</option>{folders.filter((item) => item.rawPath !== folder?.rawPath).map((item) => <option key={item.rawPath} value={item.rawPath}>{item.name}</option>)}</select>
          </div>
          <div className="reader-content"><h2>{selected.subject}</h2><div className="sender-line"><div className="sender-avatar big">{(selected.fromName || selected.fromAddress || "?")[0].toUpperCase()}</div><div><strong>{selected.fromName || selected.fromAddress}</strong><small>{selected.fromAddress}<br />Кому: {selected.to}</small></div><time>{friendlyDate(selected.date)}</time></div>
            <div className="label-editor">{knownLabels.map((name, index) => <button key={name} className={selected.labels.includes(name) ? "chosen" : ""} onClick={() => void toggleLabel(name)}><i className={`label-dot dot-${index % 5}`} />{name}</button>)}</div>
            <pre className="message-body">{selected.bodyText || "В письме нет текстовой версии."}</pre>
          </div>
        </section>}
      </div>
    </main>
    {compose && <Compose onClose={() => setCompose(false)} />}
  </div>;
}

function AccountScreen({ onSaved }: { onSaved: (account: Account) => void }) {
  const [account, setAccount] = useState(defaults); const [password, setPassword] = useState(""); const [error, setError] = useState(""); const [busy, setBusy] = useState(false);
  async function save(event: FormEvent) { event.preventDefault(); setBusy(true); setError(""); try { await invoke("save_account", { account, password }); await invoke("test_account"); onSaved(account); } catch (reason) { setError(String(reason)); } finally { setBusy(false); } }
  return <main className="account-screen"><form onSubmit={save}><div className="brand large"><span className="brand-mark">Я</span><strong>Почта</strong></div><h1>Войдите в почту</h1><p>Адрес и пароль приложения сохраняются только в защищённом хранилище компьютера.</p><label>Адрес почты<input type="email" required value={account.email} onChange={(e) => setAccount({ ...account, email: e.target.value })} placeholder="name@yandex.ru" /></label><label>Пароль приложения<input type="password" required value={password} onChange={(e) => setPassword(e.target.value)} /></label>{error && <div className="error">{error}</div>}<button className="compose-button" disabled={busy}>{busy ? "Проверяем…" : "Войти"}</button></form></main>;
}

function Compose({ onClose }: { onClose: () => void }) {
  const [to, setTo] = useState(""); const [subject, setSubject] = useState(""); const [attachments, setAttachments] = useState<string[]>([]); const [busy, setBusy] = useState(false); const [error, setError] = useState(""); const editor = useRef<HTMLDivElement>(null);
  async function attach() { const paths = await open({ multiple: true, directory: false }); if (paths) setAttachments((items) => [...items, ...(Array.isArray(paths) ? paths : [paths])]); }
  async function send() { if (!editor.current) return; setBusy(true); setError(""); try { await invoke("send_message", { outgoing: { to, subject, text: editor.current.innerText, html: editor.current.innerHTML, attachments } }); onClose(); } catch (reason) { setError(String(reason)); } finally { setBusy(false); } }
  function format(command: "bold" | "italic" | "underline") { editor.current?.focus(); document.execCommand(command); }
  return <section className="compose"><header><strong>Новое письмо</strong><button onClick={onClose}>×</button></header><input placeholder="Кому" type="email" value={to} onChange={(event) => setTo(event.target.value)} /><input placeholder="Тема" value={subject} onChange={(event) => setSubject(event.target.value)} /><div ref={editor} className="compose-editor" contentEditable data-placeholder="Напишите сообщение…" />{attachments.length > 0 && <div className="attachments">{attachments.map((path) => <button key={path} onClick={() => setAttachments((items) => items.filter((item) => item !== path))}>{path.split(/[\\/]/).pop()} ×</button>)}</div>}{error && <div className="error">{error}</div>}<footer><button className="compose-button" disabled={busy || !to} onClick={() => void send()}>{busy ? "Отправляем…" : "Отправить"}</button><button className="format" onClick={() => format("bold")} title="Жирный"><b>Ж</b></button><button className="format" onClick={() => format("italic")} title="Курсив"><i>К</i></button><button className="format" onClick={() => format("underline")} title="Подчёркнутый"><u>П</u></button><button className="format" onClick={() => void attach()} title="Прикрепить файл">⌕</button></footer></section>;
}
