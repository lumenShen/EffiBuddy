//! In-page code editor bridge.
//!
//! Web IDEs (Monaco, CodeMirror, Ace, or a bare `<textarea>`) are a common
//! target: the page holds the buffer, the site owns the save button, and the
//! accessibility snapshot sees one giant textbox. Driving them with raw
//! `Runtime.evaluate` means re-deriving the same five adapters every time, so
//! the backend ships them once:
//!
//! - `detect` / `list` — which editor family is on the page, and the files it
//!   holds (Monaco models, CodeMirror/Ace instances, textareas);
//! - `read` — the buffer text, capped to the caller's budget;
//! - `write` — replace the buffer through the editor's own API (so the site's
//!   React/Vue binding notices), without saving;
//! - `save` — `write` when content was given, then the page's save shortcut.
//!
//! Dirty tracking is deliberately local: the page is the only authority on
//! "saved", so the script remembers the buffer it last saw and compares. A
//! `save` refreshes that memory, which turns "did Ctrl+S actually persist?" into
//! a check the model can make instead of a guess.

use crate::types::{EditorOp, EditorRequest};

/// The page-side adapter. `__EFFI_EDITOR_ARG__` is replaced with the request
/// JSON; everything else is plain ES2017 (no eval, so page CSP cannot block it).
const EDITOR_JS: &str = r##"(() => {
  const request = __EFFI_EDITOR_ARG__;
  const notes = [];
  const maxChars = Number(request.max_chars) > 0 ? Number(request.max_chars) : 20000;
  const cut = (value) => {
    const text = String(value == null ? "" : value);
    if (text.length <= maxChars) return { text, truncated: false };
    return { text: text.slice(0, maxChars) + "\u2026", truncated: true };
  };
  const box = (el) => {
    if (!el || !el.getBoundingClientRect) return null;
    const rect = el.getBoundingClientRect();
    if (rect.width <= 2 || rect.height <= 2) return null;
    return rect;
  };
  const memory = (() => {
    const key = "__effi_editor_mem";
    if (!globalThis[key] || typeof globalThis[key] !== "object") globalThis[key] = Object.create(null);
    return globalThis[key];
  })();
  const remember = (target, value) => { if (target) memory[target] = value; };
  const dirtyOf = (target, value) => {
    if (!target || !(target in memory)) return null;
    return memory[target] !== value;
  };
  const pathFromElement = (el) => {
    let node = el;
    for (let depth = 0; depth < 6 && node && node.getAttribute; depth += 1, node = node.parentElement) {
      for (const attribute of ["data-path", "data-filename", "data-file", "data-name"]) {
        const value = node.getAttribute(attribute);
        if (value) return value;
      }
      const title = node.getAttribute("title");
      if (title && /\.[A-Za-z0-9]{1,8}$/.test(title.trim())) return title.trim();
    }
    return null;
  };
  const pathFromTabs = () => {
    const nodes = document.querySelectorAll('[role="tab"], .tab, .file, .nav-link, .list-item');
    for (const node of nodes) {
      if (!box(node)) continue;
      const text = (node.textContent || "").trim();
      if (/\.[A-Za-z0-9]{1,8}$/.test(text) && text.length < 120) return text;
    }
    return null;
  };
  const normalize = (files) => {
    const fallback = pathFromTabs() || null;
    files.forEach((file, index) => {
      if (!file.target) file.target = fallback ? fallback : "editor-" + index;
      if (file.target === fallback && files.length > 1) file.target = file.target + "#" + index;
    });
    return files;
  };
  const matchTarget = (files, wanted) => {
    if (wanted == null || wanted === "") {
      const current = files.findIndex((file) => file.current);
      return current >= 0 ? files[current] : files[0] || null;
    }
    const trimmed = String(wanted).trim();
    if (/^\d+$/.test(trimmed)) {
      const index = Number(trimmed);
      return files[index] || null;
    }
    const exact = files.find((file) => file.target === trimmed);
    if (exact) return exact;
    const suffix = files.find((file) => file.target && file.target.endsWith(trimmed));
    if (suffix) return suffix;
    const relaxed = files.find((file) => file.target &&
      file.target.toLowerCase().includes(trimmed.toLowerCase()));
    return relaxed || null;
  };

  const monacoApi = globalThis.monaco && globalThis.monaco.editor;
  const monacoFiles = () => {
    if (!monacoApi || typeof monacoApi.getModels !== "function") return null;
    const editors = typeof monacoApi.getEditors === "function" ? monacoApi.getEditors() : [];
    const focused = document.activeElement && document.activeElement.closest
      ? document.activeElement.closest(".monaco-editor") : null;
    const activeEditor = editors.find((editor) => focused && editor.getDomNode &&
      editor.getDomNode() === focused) || editors[0] || null;
    const active = activeEditor && activeEditor.getModel ? activeEditor.getModel() : null;
    const models = monacoApi.getModels();
    const files = models.map((model) => {
      const uri = model.uri || {};
      const target = uri.path || (typeof uri.toString === "function" ? uri.toString() : null);
      const value = model.getValue ? model.getValue() : "";
      return {
        target: target || null,
        kind: "monaco",
        language: model.getLanguageId ? model.getLanguageId() : null,
        chars: value.length,
        current: model === active || (active == null && models.length === 1),
        dirty: dirtyOf(target, value),
        value,
        set: (text) => model.setValue(text),
      };
    });
    return { kind: "monaco", files };
  };

  const codeMirrorFiveFiles = () => {
    const wrappers = Array.from(document.querySelectorAll(".CodeMirror"));
    const instances = wrappers
      .map((el) => el.CodeMirror)
      .filter((instance) => instance && typeof instance.getValue === "function");
    if (!instances.length) return null;
    const focused = wrappers.find((el) => el.contains(document.activeElement));
    const files = instances.map((instance, index) => {
      const wrapper = wrappers[index];
      const target = pathFromElement(wrapper);
      const value = instance.getValue();
      return {
        target: target || null,
        kind: "codemirror5",
        language: instance.getOption ? instance.getOption("mode") : null,
        chars: value.length,
        current: wrapper === focused || (instances.length === 1),
        dirty: dirtyOf(target, value),
        value,
        set: (text) => {
          instance.setValue(text);
          if (instance.refresh) instance.refresh();
        },
      };
    });
    return { kind: "codemirror5", files };
  };

  const codeMirrorSixFiles = () => {
    const contents = Array.from(document.querySelectorAll(".cm-editor .cm-content")).filter(box);
    if (!contents.length) return null;
    const files = contents.map((content, index) => {
      const view = content.cmView && content.cmView.view ? content.cmView.view : null;
      if (!view || !view.state) return null;
      const editor = content.closest(".cm-editor");
      const target = pathFromElement(editor);
      const value = view.state.doc.toString();
      return {
        target: target || null,
        kind: "codemirror6",
        language: null,
        chars: value.length,
        current: content.contains(document.activeElement) || contents.length === 1,
        dirty: dirtyOf(target, value),
        value,
        set: (text) => view.dispatch({
          changes: { from: 0, to: view.state.doc.length, insert: text },
        }),
        focus: () => view.focus && view.focus(),
      };
    }).filter((file) => file);
    if (!files.length) return null;
    return { kind: "codemirror6", files };
  };

  const aceFiles = () => {
    const editors = Array.from(document.querySelectorAll(".ace_editor"))
      .map((el) => (el.env && el.env.editor) || null)
      .filter((editor) => editor && typeof editor.getValue === "function");
    if (!editors.length) return null;
    const active = typeof globalThis.ace !== "undefined" && globalThis.ace.edit
      ? null : null;
    const files = editors.map((editor, index) => {
      const container = editor.container;
      const target = pathFromElement(container);
      const value = editor.getValue();
      return {
        target: target || null,
        kind: "ace",
        language: editor.session && editor.session.getMode ? editor.session.getMode().$id : null,
        chars: value.length,
        current: (container && container.contains(document.activeElement)) || (editors.length === 1),
        dirty: dirtyOf(target, value),
        value,
        set: (text) => editor.setValue(text, 0),
        focus: () => editor.focus && editor.focus(),
      };
    });
    void active;
    return { kind: "ace", files };
  };

  const textareaFiles = () => {
    const nodes = Array.from(document.querySelectorAll(
      "textarea, [contenteditable='true'], [contenteditable=''], [contenteditable='plaintext-only']"
    )).filter(box);
    if (!nodes.length) return null;
    const focused = nodes.find((node) => node === document.activeElement);
    const files = nodes.map((node, index) => {
      const target = pathFromElement(node) || pathFromTabs();
      const value = node.tagName === "TEXTAREA" ? node.value : node.innerText;
      return {
        target: target || null,
        kind: "textarea",
        language: null,
        chars: value.length,
        current: node === focused || nodes.length === 1,
        dirty: dirtyOf(target, value),
        value,
        node,
        set: (text) => {
          if (node.tagName === "TEXTAREA") {
            const setter = Object.getOwnPropertyDescriptor(
              globalThis.HTMLTextAreaElement.prototype, "value"
            ).set;
            setter.call(node, text);
            node.dispatchEvent(new Event("input", { bubbles: true }));
            node.dispatchEvent(new Event("change", { bubbles: true }));
          } else {
            node.textContent = text;
            node.dispatchEvent(new InputEvent("input", { bubbles: true }));
            node.dispatchEvent(new Event("change", { bubbles: true }));
          }
        },
        focus: () => node.focus && node.focus(),
      };
    });
    return { kind: "textarea", files };
  };

  let detected = null;
  for (const probe of [
    monacoFiles, codeMirrorFiveFiles, codeMirrorSixFiles, aceFiles, textareaFiles,
  ]) {
    const outcome = probe();
    if (outcome) { detected = outcome; break; }
  }

  if (!detected) {
    return {
      ok: false,
      kind: "none",
      files: [],
      notes: notes.concat([
        "no Monaco / CodeMirror / Ace / textarea editor found in this document",
        "if the IDE lives in an iframe, drive that frame directly",
      ]),
    };
  }

  const files = normalize(detected.files);
  const publicFiles = files.map((file, index) => ({
    index,
    target: file.target,
    kind: file.kind,
    language: file.language,
    chars: file.chars,
    current: !!file.current,
    dirty: file.dirty,
  }));

  const op = request.op || "detect";
  const answer = { ok: true, kind: detected.kind, files: publicFiles, notes };

  if (op === "detect" || op === "list") {
    return answer;
  }

  const chosen = matchTarget(files, request.target);
  if (!chosen) {
    return {
      ok: false,
      kind: detected.kind,
      files: publicFiles,
      error: "target not found: " + String(request.target) +
        "; call browser editor with op=list to see available targets",
    };
  }
  answer.target = chosen.target;
  answer.index = files.indexOf(chosen);

  if (op === "read") {
    const sliced = cut(chosen.value);
    remember(chosen.target, chosen.value);
    answer.value = sliced.text;
    answer.truncated = sliced.truncated;
    answer.chars = chosen.value.length;
    answer.dirty = chosen.dirty;
    return answer;
  }

  if (op === "write" || op === "save") {
    const hasContent = request.content != null && request.content !== undefined;
    if (!hasContent && op === "write") {
      return { ok: false, error: "op=write needs content; use op=read first to keep the buffer" };
    }
    // Keep the pre-write buffer as the "last known saved" state so the caller
    // can tell a successful save from a no-op.
    if (!(chosen.target in memory)) memory[chosen.target] = chosen.value;
    if (hasContent) {
      const content = String(request.content);
      chosen.set(content);
      if (chosen.focus) {
        try { chosen.focus(); } catch (_) {}
      }
      answer.written = true;
      answer.chars = content.length;
      answer.dirty = memory[chosen.target] !== content;
    } else {
      // `save` without content keeps the buffer and only presses the site's
      // shortcut.
      answer.written = false;
      answer.chars = chosen.value.length;
      answer.dirty = memory[chosen.target] !== chosen.value;
    }
    if (op === "save") {
      answer.needs_shortcut = true;
    }
    return answer;
  }

  return { ok: false, error: "unknown editor op: " + String(op), files: publicFiles };
})()"##;

impl EditorRequest {
    /// Render the request as the page-side script for this call.
    pub fn script(&self) -> String {
        let request = serde_json::json!({
            "op": self.op.as_str(),
            "target": self.target,
            "content": self.content,
            "max_chars": self.max_chars,
        });
        let argument = serde_json::to_string(&request).unwrap_or_else(|_| "{}".to_string());
        EDITOR_JS.replace("__EFFI_EDITOR_ARG__", &argument)
    }

    /// True when the backend must dispatch the page's save shortcut after the
    /// script ran (only `save` does).
    pub fn needs_save_shortcut(&self) -> bool {
        self.op == EditorOp::Save
    }
}

/// Script that re-reads the editor state after a save, refreshing the local
/// memory so `dirty` means "changed since the save" rather than "changed since
/// the page loaded".
pub fn post_save_script(target: Option<&str>) -> String {
    let target = serde_json::to_string(&target).unwrap_or_else(|_| "null".to_string());
    format!(
        r#"(() => {{
  const key = "__effi_editor_mem";
  const memory = globalThis[key] && typeof globalThis[key] === "object" ? globalThis[key] : null;
  if (!memory) return {{ ok: false, note: "editor memory is gone (the page navigated?)" }};
  const wanted = {target};
  const targets = Object.keys(memory);
  const chosen = wanted && targets.find((name) => name === wanted) ? wanted : targets[targets.length - 1];
  const probe = {{
    monaco: globalThis.monaco && globalThis.monaco.editor && globalThis.monaco.editor.getModels
      ? () => globalThis.monaco.editor.getModels() : null,
    cm5: document.querySelector(".CodeMirror"),
    cm6: document.querySelector(".cm-editor .cm-content"),
    ace: document.querySelector(".ace_editor"),
    textarea: document.querySelector("textarea, [contenteditable='true']"),
  }};
  let value = null;
  try {{
    if (probe.monaco) {{
      const models = probe.monaco();
      const ids = models.map((model) => (model.uri && (model.uri.path || String(model.uri))) || null);
      const index = chosen ? ids.indexOf(chosen) : -1;
      if (index >= 0) value = models[index].getValue();
    }} else if (probe.cm5 && probe.cm5.CodeMirror) {{
      value = probe.cm5.CodeMirror.getValue();
    }} else if (probe.cm6 && probe.cm6.cmView && probe.cm6.cmView.view) {{
      value = probe.cm6.cmView.view.state.doc.toString();
    }} else if (probe.ace && probe.ace.env && probe.ace.env.editor) {{
      value = probe.ace.env.editor.getValue();
    }} else if (probe.textarea) {{
      value = probe.textarea.tagName === "TEXTAREA" ? probe.textarea.value : probe.textarea.innerText;
    }}
  }} catch (error) {{
    return {{ ok: false, error: String(error) }};
  }}
  if (value == null || !chosen) return {{ ok: false, note: "cannot re-read the buffer" }};
  const changed = memory[chosen] !== value;
  memory[chosen] = value;
  return {{ ok: true, target: chosen, dirty: changed }};
}})()"#,
        target = target
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(op: EditorOp) -> EditorRequest {
        EditorRequest {
            op,
            target: Some("src/main.cpp".to_string()),
            content: Some("int main() { return 0; }".to_string()),
            max_chars: 500,
        }
    }

    #[test]
    fn script_embeds_the_request_and_detects_every_editor() {
        let script = request(EditorOp::Write).script();
        assert!(script.contains("\"op\":\"write\""), "{script}");
        assert!(script.contains("src/main.cpp"), "{script}");
        for family in ["monaco", "codemirror5", "codemirror6", "ace", "textarea"] {
            assert!(script.contains(family), "missing {family} adapter");
        }
        // No `eval`: page CSP would be able to block it.
        assert!(!script.contains("eval("), "{script}");
    }

    #[test]
    fn request_payload_is_json_escaped() {
        let mut request = request(EditorOp::Write);
        request.content = Some("line \"quoted\"\nnext".to_string());
        let script = request.script();
        assert!(script.contains(r#"line \"quoted\"\nnext"#), "{script}");
    }

    #[test]
    fn only_save_dispatchhes_the_shortcut() {
        assert!(request(EditorOp::Save).needs_save_shortcut());
        assert!(!request(EditorOp::Write).needs_save_shortcut());
        assert!(!request(EditorOp::Read).needs_save_shortcut());
    }

    #[test]
    fn post_save_script_mentions_its_target() {
        let script = post_save_script(Some("src/main.cpp"));
        assert!(script.contains("src/main.cpp"), "{script}");
        assert!(script.contains("dirty"), "{script}");
    }
}
