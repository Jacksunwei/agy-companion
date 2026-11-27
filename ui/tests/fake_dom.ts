export interface FakeRect {
  left: number;
  top: number;
  right: number;
  bottom: number;
  width: number;
  height: number;
}

export interface FakeEventInit {
  bubbles?: boolean;
  key?: string;
  isComposing?: boolean;
  keyCode?: number;
  relatedTarget?: FakeNode | null;
}

export class FakeEvent {
  type: string;
  bubbles: boolean;
  key: string;
  isComposing: boolean;
  keyCode: number;
  relatedTarget: FakeNode | null;
  target: FakeNode | null;
  currentTarget: FakeNode | null;
  defaultPrevented: boolean;
  propagationStopped: boolean;

  constructor(type: string, init: FakeEventInit = {}) {
    this.type = type;
    this.bubbles = init.bubbles ?? false;
    this.key = init.key ?? "";
    this.isComposing = init.isComposing ?? false;
    this.keyCode = init.keyCode ?? 0;
    this.relatedTarget = init.relatedTarget ?? null;
    this.target = null;
    this.currentTarget = null;
    this.defaultPrevented = false;
    this.propagationStopped = false;
  }

  stopPropagation(): void {
    this.propagationStopped = true;
  }

  preventDefault(): void {
    this.defaultPrevented = true;
  }
}

type EventListenerFn = (event: FakeEvent) => void;

interface SimpleSelector {
  tag: string | null;
  id: string | null;
  classes: string[];
  attrs: Array<{ name: string; value: string | null }>;
}

interface SelectorPart {
  combinator: " " | ">" | null;
  simple: SimpleSelector;
}

function parseSimpleSelector(raw: string): SimpleSelector {
  const result: SimpleSelector = { tag: null, id: null, classes: [], attrs: [] };
  let i = 0;
  const len = raw.length;
  while (i < len) {
    const ch = raw[i];
    if (ch === "#") {
      let j = i + 1;
      while (j < len && /[^\s.#\[]/.test(raw[j])) j += 1;
      result.id = raw.slice(i + 1, j);
      i = j;
    } else if (ch === ".") {
      let j = i + 1;
      while (j < len && /[^\s.#\[]/.test(raw[j])) j += 1;
      result.classes.push(raw.slice(i + 1, j));
      i = j;
    } else if (ch === "[") {
      const close = raw.indexOf("]", i + 1);
      if (close === -1) break;
      const body = raw.slice(i + 1, close).trim();
      const eq = body.indexOf("=");
      if (eq === -1) {
        result.attrs.push({ name: body, value: null });
      } else {
        const name = body.slice(0, eq).trim();
        let val = body.slice(eq + 1).trim();
        if (
          (val.startsWith('"') && val.endsWith('"')) ||
          (val.startsWith("'") && val.endsWith("'"))
        ) {
          val = val.slice(1, -1);
        }
        result.attrs.push({ name, value: val });
      }
      i = close + 1;
    } else {
      let j = i;
      while (j < len && /[^\s.#\[]/.test(raw[j])) j += 1;
      const tag = raw.slice(i, j);
      if (tag && tag !== "*") {
        result.tag = tag.toLowerCase();
      }
      i = j;
    }
  }
  return result;
}

function parseComplexSelector(raw: string): SelectorPart[] {
  const parts: SelectorPart[] = [];
  const tokens: string[] = [];
  let buf = "";
  let inBracket = false;
  for (let i = 0; i < raw.length; i += 1) {
    const ch = raw[i];
    if (ch === "[") inBracket = true;
    else if (ch === "]") inBracket = false;
    if (!inBracket && ch === ">") {
      if (buf.trim()) tokens.push(buf.trim());
      tokens.push(">");
      buf = "";
    } else if (!inBracket && /\s/.test(ch)) {
      if (buf.trim()) tokens.push(buf.trim());
      buf = "";
    } else {
      buf += ch;
    }
  }
  if (buf.trim()) tokens.push(buf.trim());

  let nextCombinator: " " | ">" | null = null;
  for (const tok of tokens) {
    if (tok === ">") {
      nextCombinator = ">";
    } else {
      parts.push({
        combinator: parts.length === 0 ? null : (nextCombinator ?? " "),
        simple: parseSimpleSelector(tok),
      });
      nextCombinator = null;
    }
  }
  return parts;
}

function matchesSimple(el: FakeElement, sel: SimpleSelector): boolean {
  if (sel.tag !== null && el.tagName.toLowerCase() !== sel.tag) {
    return false;
  }
  if (sel.id !== null && el.id !== sel.id) {
    return false;
  }
  for (const cls of sel.classes) {
    if (!el.classList.contains(cls)) return false;
  }
  for (const attr of sel.attrs) {
    const actual = el.getAttribute(attr.name);
    if (actual === null) return false;
    if (attr.value !== null && actual !== attr.value) return false;
  }
  return true;
}

function matchesComplex(el: FakeElement, parts: SelectorPart[]): boolean {
  if (parts.length === 0) return false;
  if (!matchesSimple(el, parts[parts.length - 1].simple)) return false;

  const matchFrom = (node: FakeElement, idx: number): boolean => {
    if (idx === 0) return true;
    const comb = parts[idx].combinator;
    const targetSimple = parts[idx - 1].simple;
    if (comb === ">") {
      const parent = node.parentNode;
      if (!(parent instanceof FakeElement)) return false;
      return matchesSimple(parent, targetSimple) && matchFrom(parent, idx - 1);
    }
    let cur = node.parentNode;
    while (cur !== null) {
      if (cur instanceof FakeElement && matchesSimple(cur, targetSimple)) {
        if (matchFrom(cur, idx - 1)) return true;
      }
      cur = cur.parentNode;
    }
    return false;
  };

  return matchFrom(el, parts.length - 1);
}

function matchesSelectorList(el: FakeElement, selector: string): boolean {
  const branches = selector
    .split(",")
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
  return branches.some((b) => matchesComplex(el, parseComplexSelector(b)));
}

export class FakeNode {
  parentNode: FakeNode | null;
  childNodes: FakeNode[];
  ownerDocument: FakeDocument | null;
  private listeners: Map<string, EventListenerFn[]>;

  constructor(ownerDocument: FakeDocument | null = null) {
    this.parentNode = null;
    this.childNodes = [];
    this.ownerDocument = ownerDocument;
    this.listeners = new Map();
  }

  get children(): FakeElement[] {
    return this.childNodes.filter((n): n is FakeElement => n instanceof FakeElement);
  }

  get textContent(): string {
    return this.childNodes.map((n) => n.textContent).join("");
  }

  set textContent(value: string | null) {
    for (const child of this.childNodes) {
      child.parentNode = null;
    }
    this.childNodes = [];
    const str = value === null || value === undefined ? "" : String(value);
    if (str.length > 0) {
      const txt = new FakeText(str, this.ownerDocument);
      txt.parentNode = this;
      this.childNodes.push(txt);
    }
  }

  appendChild<T extends FakeNode>(node: T): T {
    if (node instanceof FakeDocumentFragment) {
      const moved = [...node.childNodes];
      node.childNodes = [];
      for (const child of moved) {
        child.parentNode = null;
        this.appendChild(child);
      }
      return node;
    }
    if (node.parentNode) {
      node.remove();
    }
    node.parentNode = this;
    if (this.ownerDocument && !node.ownerDocument) {
      node.ownerDocument = this.ownerDocument;
    }
    this.childNodes.push(node);
    return node;
  }

  append(...items: Array<FakeNode | string | number>): void {
    for (const item of items) {
      if (item instanceof FakeNode) {
        this.appendChild(item);
      } else {
        this.appendChild(new FakeText(String(item), this.ownerDocument));
      }
    }
  }

  replaceChildren(...items: Array<FakeNode | string | number>): void {
    for (const child of this.childNodes) {
      child.parentNode = null;
    }
    this.childNodes = [];
    this.append(...items);
  }

  remove(): void {
    if (!this.parentNode) return;
    const idx = this.parentNode.childNodes.indexOf(this);
    if (idx !== -1) {
      this.parentNode.childNodes.splice(idx, 1);
    }
    this.parentNode = null;
  }

  contains(target: FakeNode | null | undefined): boolean {
    let cur: FakeNode | null | undefined = target;
    while (cur) {
      if (cur === this) return true;
      cur = cur.parentNode;
    }
    return false;
  }

  addEventListener(type: string, fn: EventListenerFn): void {
    let list = this.listeners.get(type);
    if (!list) {
      list = [];
      this.listeners.set(type, list);
    }
    if (!list.includes(fn)) {
      list.push(fn);
    }
  }

  removeEventListener(type: string, fn: EventListenerFn): void {
    const list = this.listeners.get(type);
    if (!list) return;
    const idx = list.indexOf(fn);
    if (idx !== -1) {
      list.splice(idx, 1);
    }
  }

  dispatchEvent(event: FakeEvent): boolean {
    event.target = this;
    let cur: FakeNode | null = this;
    while (cur !== null) {
      event.currentTarget = cur;
      const list = cur.listeners.get(event.type);
      if (list && list.length > 0) {
        for (const fn of [...list]) {
          fn(event);
        }
      }
      if (!event.bubbles || event.propagationStopped) break;
      cur = cur.parentNode;
    }
    return !event.defaultPrevented;
  }

  querySelectorAll(selector: string): FakeElement[] {
    const out: FakeElement[] = [];
    const visit = (node: FakeNode) => {
      for (const child of node.childNodes) {
        if (child instanceof FakeElement) {
          if (matchesSelectorList(child, selector)) {
            out.push(child);
          }
          visit(child);
        }
      }
    };
    visit(this);
    return out;
  }

  querySelector(selector: string): FakeElement | null {
    return this.querySelectorAll(selector)[0] ?? null;
  }
}

export class FakeText extends FakeNode {
  data: string;

  constructor(data: string, ownerDocument: FakeDocument | null = null) {
    super(ownerDocument);
    this.data = data;
  }

  override get textContent(): string {
    return this.data;
  }

  override set textContent(value: string | null) {
    this.data = value === null || value === undefined ? "" : String(value);
  }
}

export class FakeDocumentFragment extends FakeNode {
  constructor(ownerDocument: FakeDocument | null = null) {
    super(ownerDocument);
  }
}

export class FakeElement extends FakeNode {
  tagName: string;
  namespaceURI: string | null;
  hidden: boolean;
  disabled: boolean;
  open: boolean;
  checked: boolean;
  value: string;
  placeholder: string;
  autocomplete: string;
  spellcheck: boolean;
  scrollTop: number;
  style: Record<string, string> & {
    setProperty: (k: string, v: string) => void;
    getPropertyValue: (k: string) => string;
  };
  rect: FakeRect;
  readonly classList: {
    add: (...tokens: string[]) => void;
    remove: (...tokens: string[]) => void;
    contains: (token: string) => boolean;
    toggle: (token: string, force?: boolean) => boolean;
  };
  private attrs: Map<string, string>;

  constructor(
    tag: string,
    ownerDocument: FakeDocument | null = null,
    namespaceURI: string | null = null,
  ) {
    super(ownerDocument);
    this.tagName = namespaceURI ? tag : tag.toUpperCase();
    this.namespaceURI = namespaceURI;
    this.hidden = false;
    this.disabled = false;
    this.open = false;
    this.checked = false;
    this.value = "";
    this.placeholder = "";
    this.autocomplete = "";
    this.spellcheck = false;
    this.scrollTop = 0;
    this.attrs = new Map();
    this.rect = { left: 0, top: 0, right: 100, bottom: 30, width: 100, height: 30 };

    const styleObj = {
      setProperty(k: string, v: string): void {
        (styleObj as Record<string, unknown>)[k] = v;
      },
      getPropertyValue(k: string): string {
        const val = (styleObj as Record<string, unknown>)[k];
        return typeof val === "string" ? val : "";
      },
    } as FakeElement["style"];
    this.style = styleObj;

    this.classList = {
      add: (...tokens: string[]) => {
        const set = new Set(this.getClassTokens());
        for (const t of tokens) {
          if (t) set.add(t);
        }
        this.className = [...set].join(" ");
      },
      remove: (...tokens: string[]) => {
        const set = new Set(this.getClassTokens());
        for (const t of tokens) {
          set.delete(t);
        }
        this.className = [...set].join(" ");
      },
      contains: (token: string) => this.getClassTokens().includes(token),
      toggle: (token: string, force?: boolean) => {
        const has = this.classList.contains(token);
        const next = force !== undefined ? force : !has;
        if (next) this.classList.add(token);
        else this.classList.remove(token);
        return next;
      },
    };
  }

  private getClassTokens(): string[] {
    const raw = this.attrs.get("class") ?? "";
    return raw.trim() ? raw.trim().split(/\s+/) : [];
  }

  get className(): string {
    return this.attrs.get("class") ?? "";
  }

  set className(value: string) {
    const str = String(value);
    if (str.length === 0) {
      this.attrs.delete("class");
    } else {
      this.attrs.set("class", str);
    }
  }

  get id(): string {
    return this.attrs.get("id") ?? "";
  }

  set id(value: string) {
    const str = String(value);
    if (str.length === 0) {
      this.attrs.delete("id");
    } else {
      this.attrs.set("id", str);
    }
  }

  get title(): string {
    return this.attrs.get("title") ?? "";
  }

  set title(value: string) {
    const str = String(value);
    if (str.length === 0) {
      this.attrs.delete("title");
    } else {
      this.attrs.set("title", str);
    }
  }

  get name(): string {
    return this.attrs.get("name") ?? "";
  }

  set name(value: string) {
    const str = String(value);
    if (str.length === 0) {
      this.attrs.delete("name");
    } else {
      this.attrs.set("name", str);
    }
  }

  get type(): string {
    return this.attrs.get("type") ?? "";
  }

  set type(value: string) {
    const str = String(value);
    if (str.length === 0) {
      this.attrs.delete("type");
    } else {
      this.attrs.set("type", str);
    }
  }

  get htmlFor(): string {
    return this.attrs.get("for") ?? "";
  }

  set htmlFor(value: string) {
    const str = String(value);
    if (str.length === 0) {
      this.attrs.delete("for");
    } else {
      this.attrs.set("for", str);
    }
  }

  get tabIndex(): number {
    const raw = this.attrs.get("tabindex");
    if (raw === undefined) return -1;
    const parsed = Number.parseInt(raw, 10);
    return Number.isNaN(parsed) ? -1 : parsed;
  }

  set tabIndex(value: number) {
    this.attrs.set("tabindex", String(Math.trunc(value)));
  }

  setAttribute(name: string, value: string): void {
    this.attrs.set(name, String(value));
  }

  getAttribute(name: string): string | null {
    return this.attrs.get(name) ?? null;
  }

  getAttributeNames(): string[] {
    return [...this.attrs.keys()];
  }

  removeAttribute(name: string): void {
    this.attrs.delete(name);
  }

  hasAttribute(name: string): boolean {
    return this.attrs.has(name);
  }

  getBoundingClientRect(): FakeRect {
    return { ...this.rect };
  }

  closest(selector: string): FakeElement | null {
    let cur: FakeNode | null = this;
    while (cur) {
      if (cur instanceof FakeElement && matchesSelectorList(cur, selector)) {
        return cur;
      }
      cur = cur.parentNode;
    }
    return null;
  }

  focus(): void {
    if (this.ownerDocument) {
      this.ownerDocument.activeElement = this;
    }
    this.dispatchEvent(new FakeEvent("focus", { bubbles: false }));
  }

  blur(relatedTarget: FakeNode | null = null): void {
    if (this.ownerDocument && this.ownerDocument.activeElement === this) {
      this.ownerDocument.activeElement = null;
    }
    this.dispatchEvent(new FakeEvent("blur", { bubbles: false, relatedTarget }));
    this.dispatchEvent(new FakeEvent("focusout", { bubbles: true, relatedTarget }));
  }

  select(): void {
    // No-op for offscreen textarea copy path
  }

  click(): void {
    if (this.disabled) return;
    this.dispatchEvent(new FakeEvent("click", { bubbles: true }));
  }
}

export class FakeDocument extends FakeNode {
  body: FakeElement;
  activeElement: FakeElement | null;
  hidden: boolean;
  execCommandResult: boolean;

  constructor() {
    super(null);
    this.ownerDocument = this;
    this.body = new FakeElement("body", this);
    this.body.parentNode = this;
    this.childNodes.push(this.body);
    this.activeElement = null;
    this.hidden = false;
    this.execCommandResult = false;
  }

  createElement(tag: string): FakeElement {
    return new FakeElement(tag, this);
  }

  createElementNS(ns: string, tag: string): FakeElement {
    return new FakeElement(tag, this, ns);
  }

  createDocumentFragment(): FakeDocumentFragment {
    return new FakeDocumentFragment(this);
  }

  getElementById(id: string): FakeElement | null {
    return this.querySelector(`#${id}`);
  }

  execCommand(_commandId: string): boolean {
    return this.execCommandResult;
  }
}

interface PendingTimer {
  id: number;
  fn: () => void;
  delay: number;
}

export interface FakeDomEnv {
  document: FakeDocument;
  window: {
    setTimeout: (fn: () => void, delay?: number) => number;
    clearTimeout: (id: number) => void;
  };
  flushTimers: () => number;
  pendingTimerCount: () => number;
  mockClipboard: (writeText?: (text: string) => Promise<void>) => void;
  mockUserAgent: (userAgent: string) => void;
  teardown: () => void;
}

export function installFakeDom(): FakeDomEnv {
  const g = globalThis as Record<string, unknown>;
  const savedDescriptors = new Map<string, PropertyDescriptor | undefined>();
  const touchGlobal = (key: string, value: unknown): void => {
    if (!savedDescriptors.has(key)) {
      savedDescriptors.set(key, Object.getOwnPropertyDescriptor(globalThis, key));
    }
    Object.defineProperty(globalThis, key, {
      value,
      configurable: true,
      enumerable: true,
      writable: true,
    });
  };

  const doc = new FakeDocument();
  let nextTimerId = 1;
  const timers = new Map<number, PendingTimer>();

  const fakeWindow = {
    setTimeout: (fn: () => void, delay = 0): number => {
      const id = nextTimerId;
      nextTimerId += 1;
      timers.set(id, { id, fn, delay });
      return id;
    },
    clearTimeout: (id: number): void => {
      timers.delete(id);
    },
  };

  touchGlobal("Node", FakeNode);
  touchGlobal("Element", FakeElement);
  touchGlobal("HTMLElement", FakeElement);
  touchGlobal("SVGElement", FakeElement);
  touchGlobal("DocumentFragment", FakeDocumentFragment);
  touchGlobal("Event", FakeEvent);
  touchGlobal("document", doc);
  touchGlobal("window", fakeWindow);

  let currentUa = typeof navigator !== "undefined" ? navigator.userAgent : "Node.js/24";
  let currentClipboard: { writeText: (text: string) => Promise<void> } | undefined = undefined;

  const applyNavigator = (): void => {
    const navObj: Record<string, unknown> = { userAgent: currentUa };
    if (currentClipboard) {
      navObj.clipboard = currentClipboard;
    }
    touchGlobal("navigator", navObj);
  };

  return {
    document: doc,
    window: fakeWindow,
    flushTimers: (): number => {
      const batch = [...timers.values()];
      timers.clear();
      for (const entry of batch) {
        entry.fn();
      }
      return batch.length;
    },
    pendingTimerCount: (): number => timers.size,
    mockClipboard: (writeText?: (text: string) => Promise<void>): void => {
      currentClipboard = writeText ? { writeText } : undefined;
      applyNavigator();
    },
    mockUserAgent: (userAgent: string): void => {
      currentUa = userAgent;
      applyNavigator();
    },
    teardown: (): void => {
      timers.clear();
      for (const [key, desc] of savedDescriptors.entries()) {
        if (desc === undefined) {
          delete g[key];
        } else {
          Object.defineProperty(globalThis, key, desc);
        }
      }
      savedDescriptors.clear();
    },
  };
}
