// Rendering through Steam's components: wrap a React element's type so the component's render
// output passes through a handler, for every kind of component Steam uses.
//
// A class component gets a SUBCLASS, not a prototype patch: Steam's page components are MobX
// observers, and MobX installs a read-only, non-configurable reactive `render` on each instance
// at its first render. That property can only be wrapped while it is being defined, so the
// subclass's first render watches defineProperty for its own `render`. A plain function wrapper
// around a class would throw at construction. Function, memo and forwardRef components get
// wrapped copies. Originals are never mutated; one wrapper per type keeps React's type stable.
import { diag } from "../lib/diag";

/** A short name for a React element, for the trace. */
export function describe(el: any): string {
  const t = el?.type;
  if (!t) {
    return String(el);
  }
  if (typeof t === "string") {
    return `<${t} ${String(el.props?.className ?? "").split(" ")[0]}>`;
  }
  if (typeof t === "function") {
    return `${t.displayName || t.name || "fn"}${t.prototype?.isReactComponent ? "(class)" : "(fn)"}`;
  }
  return t.render ? `fwd:${t.render.name || "?"}` : t.type ? `memo:${t.type.name || "?"}` : "obj";
}

/** Every element under `node` matching `pred`, not descending into a match (its subtree is
 *  rendered by the match itself). Bounded, because Steam's trees are deep and this runs per
 *  render. */
export function collectElements(node: any, pred: (x: any) => boolean, out: any[] = [], depth = 0): any[] {
  if (!node || depth > 40) {
    return out;
  }
  if (Array.isArray(node)) {
    for (const n of node) {
      collectElements(n, pred, out, depth + 1);
    }
    return out;
  }
  if (typeof node !== "object") {
    return out;
  }
  if (pred(node)) {
    out.push(node);
    return out;
  }
  const children = node.props?.children;
  if (children) {
    collectElements(children, pred, out, depth + 1);
  }
  return out;
}

/** Run the original with the same `this` and arguments, naming it in the trace if it throws. */
export function callOriginal(fn: any, self: unknown, args: any[], what: string): any {
  try {
    return fn.apply(self, args);
  } catch (e) {
    diag(`patch: ${what} threw: ${e instanceof Error ? e.message : String(e)}`);
    throw e;
  }
}

/** Sees a component's render output; may mutate it in place or return a replacement. `self`
 *  is the class instance for class components, otherwise undefined. */
export type RenderHandler = (out: any, self: unknown, args: any[]) => any;

export interface RenderPatcher {
  /** Route this element's component render through the handler (one wrap per type). */
  patch(el: any): void;
  /** Forget the wrappers — for dismount. */
  reset(): void;
}

export function createRenderPatcher(handler: RenderHandler, label: string): RenderPatcher {
  const wrappedTypes = new Map<any, any>();

  // Never throws: an exception in a render is Steam's error screen for the whole page.
  const guarded = (out: any, self: unknown, args: any[]): any => {
    try {
      const r = handler(out, self, args);
      return r === undefined ? out : r;
    } catch (e) {
      diag(`${label}: handler failed: ${e}`);
      return out;
    }
  };

  function wrapClass(orig: any): any {
    class Wrapped extends orig {
      render(): any {
        const define = Object.defineProperty;
        const self = this;
        (Object as any).defineProperty = function (o: any, k: PropertyKey, d: PropertyDescriptor) {
          if (o === self && k === "render" && d && typeof d.value === "function" && !(d.value as any).__punktfunk) {
            const inner = d.value;
            const reactive = function (this: unknown) {
              return guarded(inner.call(this), this, []);
            };
            Object.setPrototypeOf(reactive, inner); // MobX finds its reaction on the render itself
            (reactive as any).__punktfunk = true;
            d = { ...d, value: reactive };
          }
          return define.call(Object, o, k, d);
        };
        try {
          return guarded(callOriginal(super.render, this, [], describe({ type: orig })), this, []);
        } finally {
          (Object as any).defineProperty = define;
        }
      }
    }
    (Wrapped as any).__punktfunk = true;
    try {
      Object.defineProperty(Wrapped, "name", { value: orig.name });
    } catch {
      /* cosmetic */
    }
    return Wrapped;
  }

  function patch(el: any): void {
    const orig = el?.type;
    if (!orig) {
      return;
    }
    const cached = wrappedTypes.get(orig);
    if (cached) {
      el.type = cached;
      return;
    }
    let wrapped: any = orig;
    if (typeof orig === "function") {
      if (orig.prototype?.isReactComponent) {
        wrapped = wrapClass(orig);
      } else {
        const fn = function (this: unknown, ...args: any[]) {
          return guarded(callOriginal(orig, this, args, describe({ type: orig })), undefined, args);
        };
        Object.assign(fn, orig);
        (fn as any).__punktfunk = true;
        wrapped = fn;
      }
    } else if (typeof orig === "object") {
      if (typeof orig.render === "function") {
        const render = orig.render;
        wrapped = {
          ...orig,
          __punktfunk: true,
          render: function (this: unknown, ...args: any[]) {
            return guarded(callOriginal(render, this, args, describe({ type: orig })), undefined, args);
          },
        };
      } else if (orig.type) {
        const inner = { type: orig.type };
        patch(inner);
        wrapped = { ...orig, __punktfunk: true, type: inner.type };
      }
    }
    wrappedTypes.set(orig, wrapped);
    el.type = wrapped;
  }

  return { patch, reset: () => wrappedTypes.clear() };
}
