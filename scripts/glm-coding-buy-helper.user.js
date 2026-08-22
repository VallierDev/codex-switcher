// ==UserScript==
// @name         智谱 GLM Coding 特惠订购助手
// @name:en      Zhipu GLM Coding Buy Helper
// @namespace    http://tampermonkey.net/
// @version      6.7.0
// @description  前端辅助：拦截 JSON/Fetch/XHR 中的售罄态、解除购买按钮 disabled，并尝试绕过 Vue 组件禁用点击。仅改浏览器表现，后端库存/风控仍以官方为准。邀请码新购可减 5%：https://www.bigmodel.cn/glm-coding?ic=EVDHUUYDNB
// @description:en Frontend helper: unlock sold-out UI flags and disabled buy buttons on bigmodel.cn. Does not change backend stock/risk rules.
// @author       xiaojian
// @match        *://www.bigmodel.cn/*
// @match        *://bigmodel.cn/*
// @match        *://*.bigmodel.cn/*
// @run-at       document-start
// @grant        none
// @license      MIT
// ==/UserScript==

(function () {
  'use strict';

  // ---------------------------------------------------------------------------
  // Config
  // ---------------------------------------------------------------------------
  const CFG = {
    log: true,
    /** only rewrite responses that look like product / inventory payloads */
    strictPayload: true,
    /** unlock all buttons, or only buy-ish ones */
    unlockAllButtons: false,
    /** text patterns that mark a buy button */
    buyTextRe: /立即购买|马上购买|立即订购|去支付|确认支付|提交订单|抢购|购买|订购|下单|Buy|Checkout|Pay/i,
    stockKeys: [
      'isSoldOut', 'soldOut', 'disabled', 'available', 'isAvailable', 'canBuy',
      'stock', 'inventory', 'inventory_status', 'buy_status', 'sellStatus',
      'buyStatus', 'status', 'state', 'amount', 'discount_amount', 'price', 'name',
    ],
    soldOutTextRe: /抢购人数过多|售罄|已售罄|补货中|暂时无法购买/,
  };

  const TAG = '[GLM抢购助手]';
  const log = (...args) => {
    if (CFG.log) console.log(TAG, ...args);
  };

  // ---------------------------------------------------------------------------
  // Shared deep rewrite
  // ---------------------------------------------------------------------------
  function looksLikeProductPayload(obj) {
    if (!obj || typeof obj !== 'object') return false;
    let hits = 0;
    for (const k of CFG.stockKeys) {
      if (Object.prototype.hasOwnProperty.call(obj, k)) hits += 1;
      if (hits >= 2) return true;
    }
    // nested list of products
    if (Array.isArray(obj) && obj.length && typeof obj[0] === 'object') {
      return looksLikeProductPayload(obj[0]);
    }
    if (obj.data && typeof obj.data === 'object') {
      return looksLikeProductPayload(obj.data);
    }
    return false;
  }

  function rewriteSoldOutText(s) {
    return String(s)
      .replace(/抢购人数过多[，,]?请刷新再试/g, '立即购买')
      .replace(/抢购人数过多/g, '立即购买')
      .replace(/已售罄|售罄/g, '有货')
      .replace(/补货中|暂时无法购买/g, '立即购买');
  }

  function isProductish(obj) {
    return (
      obj.amount !== undefined ||
      obj.discount_amount !== undefined ||
      obj.price !== undefined ||
      obj.name !== undefined ||
      obj.productId !== undefined ||
      obj.product_id !== undefined ||
      obj.skuId !== undefined ||
      obj.sku_id !== undefined
    );
  }

  /**
   * @returns {boolean} whether anything changed
   */
  function deepModify(obj, seen) {
    if (!obj || typeof obj !== 'object') return false;
    seen = seen || new WeakSet();
    if (seen.has(obj)) return false;
    seen.add(obj);

    let modified = false;

    if (obj.isSoldOut === true) {
      obj.isSoldOut = false;
      modified = true;
    }
    if (obj.soldOut === true) {
      obj.soldOut = false;
      modified = true;
    }
    if (obj.disabled === true && isProductish(obj)) {
      obj.disabled = false;
      modified = true;
    }
    if (obj.available === false) {
      obj.available = true;
      modified = true;
    }
    if (obj.isAvailable === false) {
      obj.isAvailable = true;
      modified = true;
    }
    if (obj.canBuy === false) {
      obj.canBuy = true;
      modified = true;
    }
    if (obj.stock === 0 || obj.stock === '0') {
      obj.stock = 999;
      modified = true;
    }
    if (obj.inventory === 0 || obj.inventory === '0') {
      obj.inventory = 999;
      modified = true;
    }
    if (obj.inventory_status !== undefined && obj.inventory_status !== 1) {
      obj.inventory_status = 1;
      modified = true;
    }
    if (obj.buy_status !== undefined && obj.buy_status !== 1) {
      obj.buy_status = 1;
      modified = true;
    }

    if (isProductish(obj)) {
      for (const k of ['status', 'state', 'sellStatus', 'buyStatus']) {
        if (typeof obj[k] === 'number' && obj[k] !== 1) {
          obj[k] = 1;
          modified = true;
        }
      }
    }

    if (Array.isArray(obj)) {
      for (let i = 0; i < obj.length; i++) {
        if (deepModify(obj[i], seen)) modified = true;
      }
      return modified;
    }

    for (const key of Object.keys(obj)) {
      const val = obj[key];
      if (val && typeof val === 'object') {
        if (deepModify(val, seen)) modified = true;
      } else if (typeof val === 'string' && CFG.soldOutTextRe.test(val)) {
        const next = rewriteSoldOutText(val);
        if (next !== val) {
          obj[key] = next;
          modified = true;
        }
      }
    }
    return modified;
  }

  function maybeModifyParsed(result) {
    if (!result || typeof result !== 'object') return { result, modified: false };
    if (CFG.strictPayload && !looksLikeProductPayload(result)) {
      // still try top-level common envelopes
      const data = result.data || result.result || result.payload;
      if (!data || !looksLikeProductPayload(data)) {
        return { result, modified: false };
      }
    }
    const modified = deepModify(result);
    return { result, modified };
  }

  function regexRewriteJsonText(text) {
    if (
      !/"isSoldOut"\s*:\s*true/.test(text) &&
      !/"soldOut"\s*:\s*true/.test(text) &&
      !/"disabled"\s*:\s*true/.test(text) &&
      !/"stock"\s*:\s*0/.test(text) &&
      !/"status"\s*:\s*[02345]/.test(text) &&
      !CFG.soldOutTextRe.test(text)
    ) {
      return null;
    }
    return text
      .replace(/"isSoldOut"\s*:\s*true/g, '"isSoldOut":false')
      .replace(/"soldOut"\s*:\s*true/g, '"soldOut":false')
      .replace(/"disabled"\s*:\s*true/g, '"disabled":false')
      .replace(/"available"\s*:\s*false/g, '"available":true')
      .replace(/"canBuy"\s*:\s*false/g, '"canBuy":true')
      .replace(/"stock"\s*:\s*0/g, '"stock":999')
      .replace(/"inventory"\s*:\s*0/g, '"inventory":999')
      .replace(/"status"\s*:\s*[02345]/g, '"status":1')
      .replace(/"state"\s*:\s*[02345]/g, '"state":1')
      .replace(/抢购人数过多[，,]?请刷新再试/g, '立即购买')
      .replace(/抢购人数过多/g, '立即购买')
      .replace(/已售罄|售罄/g, '有货');
  }

  // ---------------------------------------------------------------------------
  // Tactic 1: JSON.parse (early SSR / embedded state)
  // ---------------------------------------------------------------------------
  const originalJSONParse = JSON.parse;
  JSON.parse = function (text, reviver) {
    const result = originalJSONParse(text, reviver);
    try {
      const { modified } = maybeModifyParsed(result);
      if (modified) log('JSON.parse payload rewritten');
    } catch (_) {
      /* ignore */
    }
    return result;
  };

  // ---------------------------------------------------------------------------
  // Tactic 2: fetch
  // ---------------------------------------------------------------------------
  const originalFetch = window.fetch;
  window.fetch = async function (...args) {
    const response = await originalFetch.apply(this, args);
    const contentType = response.headers.get('content-type') || '';
    if (!contentType.includes('application/json')) return response;

    try {
      const clone = response.clone();
      let text = await clone.text();
      let out = null;

      try {
        const jsonObj = originalJSONParse(text);
        const { modified } = maybeModifyParsed(jsonObj);
        if (modified) {
          out = JSON.stringify(jsonObj);
          log('fetch JSON rewritten', typeof args[0] === 'string' ? args[0] : args[0]?.url);
        }
      } catch (_) {
        out = regexRewriteJsonText(text);
        if (out) log('fetch JSON regex rewritten', typeof args[0] === 'string' ? args[0] : args[0]?.url);
      }

      if (out != null) {
        return new Response(out, {
          status: response.status,
          statusText: response.statusText,
          headers: response.headers,
        });
      }
    } catch (_) {
      /* ignore */
    }
    return response;
  };

  // ---------------------------------------------------------------------------
  // Tactic 3: XHR
  // ---------------------------------------------------------------------------
  const originalXHROpen = XMLHttpRequest.prototype.open;
  const originalXHRSend = XMLHttpRequest.prototype.send;

  XMLHttpRequest.prototype.open = function (method, url, ...rest) {
    this.__glmReqUrl = url;
    return originalXHROpen.call(this, method, url, ...rest);
  };

  XMLHttpRequest.prototype.send = function (...args) {
    this.addEventListener('readystatechange', function () {
      if (this.readyState !== 4 || this.status !== 200) return;
      const contentType = this.getResponseHeader('content-type') || '';
      if (!contentType.includes('application/json')) return;

      try {
        let text = this.responseText;
        let out = null;

        try {
          const jsonObj = originalJSONParse(text);
          const { modified } = maybeModifyParsed(jsonObj);
          if (modified) {
            out = JSON.stringify(jsonObj);
            log('XHR JSON rewritten', this.__glmReqUrl);
          }
        } catch (_) {
          out = regexRewriteJsonText(text);
          if (out) log('XHR JSON regex rewritten', this.__glmReqUrl);
        }

        if (out != null) {
          const finalText = out;
          Object.defineProperty(this, 'responseText', {
            configurable: true,
            get() {
              return finalText;
            },
          });
          Object.defineProperty(this, 'response', {
            configurable: true,
            get() {
              try {
                return originalJSONParse(finalText);
              } catch (_) {
                return finalText;
              }
            },
          });
        }
      } catch (_) {
        /* ignore */
      }
    });
    return originalXHRSend.apply(this, args);
  };

  // ---------------------------------------------------------------------------
  // Tactic 4: DOM unlock + Vue click bypass
  // ---------------------------------------------------------------------------
  function isBuyButton(btn) {
    if (CFG.unlockAllButtons) return true;
    const text = (btn.innerText || btn.textContent || btn.getAttribute('aria-label') || '').trim();
    if (CFG.buyTextRe.test(text)) return true;
    const cls = (btn.className && String(btn.className)) || '';
    if (/buy|purchase|order|pay|submit/i.test(cls)) return true;
    return false;
  }

  function unlockButton(btn) {
    if (!isBuyButton(btn)) return;

    if (btn.disabled || btn.hasAttribute('disabled')) {
      btn.disabled = false;
      btn.removeAttribute('disabled');
    }
    if (btn.getAttribute('aria-disabled') === 'true') {
      btn.setAttribute('aria-disabled', 'false');
    }

    const classList = Array.from(btn.classList || []);
    for (const c of classList) {
      const lower = c.toLowerCase();
      if (lower.includes('disabled') || lower.includes('is-disabled') || lower.includes('soldout')) {
        btn.classList.remove(c);
      }
    }

    if (btn.style && btn.style.pointerEvents === 'none') {
      btn.style.setProperty('pointer-events', 'auto', 'important');
    }
    if (btn.style && /grayscale|opacity:\s*0\.[0-5]/i.test(btn.getAttribute('style') || '')) {
      btn.style.setProperty('opacity', '1', 'important');
      btn.style.setProperty('filter', 'none', 'important');
    }

    try {
      for (const k of Object.keys(btn)) {
        if (k.startsWith('__vnode')) {
          const vnode = btn[k];
          const comp = vnode && vnode.component;
          if (comp) {
            if (comp.props && comp.props.disabled) comp.props.disabled = false;
            if (comp.setupState && comp.setupState.disabled) comp.setupState.disabled = false;
            if (comp.ctx && comp.ctx.disabled) comp.ctx.disabled = false;
          }
        } else if (k.startsWith('__vue__')) {
          const vueIns = btn[k];
          if (vueIns && vueIns.disabled) vueIns.disabled = false;
        }
      }
    } catch (_) {
      /* ignore */
    }
  }

  function invokeVueClick(btn, e) {
    try {
      for (const k of Object.keys(btn)) {
        if (k.startsWith('__vnode')) {
          const vnode = btn[k];
          const comp = vnode && vnode.component;
          if (comp && comp.vnode && comp.vnode.props) {
            const onClick = comp.vnode.props.onClick || comp.vnode.props.onclick;
            if (onClick) {
              log('force Vue3 parent onClick');
              e.preventDefault();
              e.stopPropagation();
              if (Array.isArray(onClick)) onClick.forEach((fn) => fn(e));
              else onClick(e);
              return true;
            }
          }
        } else if (k.startsWith('__vue__')) {
          const vueIns = btn[k];
          if (vueIns && vueIns.$listeners && vueIns.$listeners.click) {
            log('force Vue2 click');
            e.preventDefault();
            e.stopPropagation();
            const handler = vueIns.$listeners.click;
            if (Array.isArray(handler)) handler.forEach((fn) => fn(e));
            else handler(e);
            return true;
          }
        }
      }
    } catch (_) {
      /* ignore */
    }
    return false;
  }

  let unlockScheduled = false;
  function scheduleUnlock() {
    if (unlockScheduled) return;
    unlockScheduled = true;
    requestAnimationFrame(() => {
      unlockScheduled = false;
      document
        .querySelectorAll('button, [role="button"], .btn, .arco-btn, .el-button')
        .forEach(unlockButton);
    });
  }

  function startDOMObserver() {
    if (!document.documentElement) {
      setTimeout(startDOMObserver, 50);
      return;
    }

    // Capture-phase click: try to bypass component-level disabled guards
    document.addEventListener(
      'click',
      function (e) {
        const btn = e.target && e.target.closest && e.target.closest('.el-button, button, .btn, .arco-btn, [role="button"]');
        if (!btn || !isBuyButton(btn)) return;
        unlockButton(btn);
        // Only force Vue path if still looks disabled-ish
        const stillBlocked =
          btn.classList.contains('is-disabled') ||
          btn.hasAttribute('disabled') ||
          btn.getAttribute('aria-disabled') === 'true';
        if (stillBlocked) invokeVueClick(btn, e);
      },
      true
    );

    const root = document.body || document.documentElement;
    const observer = new MutationObserver(scheduleUnlock);
    observer.observe(root, {
      childList: true,
      subtree: true,
      attributes: true,
      attributeFilter: ['disabled', 'class', 'style', 'aria-disabled'],
    });

    scheduleUnlock();
    log('DOM observer + click interceptor ready');
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', startDOMObserver);
  } else {
    startDOMObserver();
  }

  log('v6.7.0 network hooks installed at document-start');
})();
