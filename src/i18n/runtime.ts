export interface UiLocale {
  code: string;
  title: string;
  translate(value: string): string;
}

const translatedMark = 'data-ui-locale';

function translateTextNode(node: Text, locale: UiLocale) {
  const parent = node.parentElement;
  if (!parent || parent.closest('script, style, textarea, input, code, pre')) return;
  const translated = locale.translate(node.data);
  if (translated !== node.data) node.data = translated;
}

function translateElementAttributes(element: Element, locale: UiLocale) {
  for (const attr of ['title', 'placeholder', 'aria-label', 'alt']) {
    const value = element.getAttribute(attr);
    if (!value) continue;
    const translated = locale.translate(value);
    if (translated !== value) element.setAttribute(attr, translated);
  }
}

function translateTree(root: ParentNode, locale: UiLocale) {
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  let node = walker.nextNode();
  while (node) {
    translateTextNode(node as Text, locale);
    node = walker.nextNode();
  }
  if (root instanceof Element) translateElementAttributes(root, locale);
  root.querySelectorAll?.('[title], [placeholder], [aria-label], [alt]')
    .forEach(element => translateElementAttributes(element, locale));
}

function wrapDialogs(locale: UiLocale) {
  const originalAlert = window.alert.bind(window);
  const originalConfirm = window.confirm.bind(window);
  const originalPrompt = window.prompt.bind(window);
  window.alert = (message?: unknown) => originalAlert(locale.translate(String(message ?? '')));
  window.confirm = (message?: string) => originalConfirm(locale.translate(String(message ?? '')));
  window.prompt = (message?: string, defaultValue?: string) => (
    originalPrompt(locale.translate(String(message ?? '')), defaultValue)
  );
}

function wrapNotification(locale: UiLocale) {
  if (!('Notification' in window)) return;
  const OriginalNotification = window.Notification;
  const LocalizedNotification = function Notification(title: string, options?: NotificationOptions) {
    const localizedOptions = options
      ? { ...options, body: options.body ? locale.translate(options.body) : options.body }
      : options;
    return new OriginalNotification(locale.translate(title), localizedOptions);
  } as unknown as typeof Notification;
  Object.setPrototypeOf(LocalizedNotification, OriginalNotification);
  LocalizedNotification.prototype = OriginalNotification.prototype;
  try {
    window.Notification = LocalizedNotification;
  } catch {
    // Some WebView builds expose Notification as read-only.
  }
}

export function installUiLocale(locale: UiLocale) {
  document.documentElement.lang = locale.code;
  document.title = locale.title;

  const bootStyle = document.createElement('style');
  bootStyle.id = 'ui-locale-boot';
  bootStyle.textContent = `body:not([${translatedMark}="ready"]) { visibility: hidden; }`;
  document.head.appendChild(bootStyle);

  wrapDialogs(locale);
  wrapNotification(locale);

  const observer = new MutationObserver((mutations) => {
    for (const mutation of mutations) {
      if (mutation.type === 'characterData' && mutation.target.nodeType === Node.TEXT_NODE) {
        translateTextNode(mutation.target as Text, locale);
        continue;
      }
      for (const node of mutation.addedNodes) {
        if (node.nodeType === Node.TEXT_NODE) {
          translateTextNode(node as Text, locale);
        } else if (node.nodeType === Node.ELEMENT_NODE) {
          translateTree(node as Element, locale);
        }
      }
      if (mutation.type === 'attributes' && mutation.target instanceof Element) {
        translateElementAttributes(mutation.target, locale);
      }
    }
  });

  observer.observe(document.documentElement, {
    childList: true,
    subtree: true,
    characterData: true,
    attributes: true,
    attributeFilter: ['title', 'placeholder', 'aria-label', 'alt'],
  });

  queueMicrotask(() => {
    translateTree(document.body, locale);
    document.body.setAttribute(translatedMark, 'ready');
    bootStyle.remove();
  });
}
