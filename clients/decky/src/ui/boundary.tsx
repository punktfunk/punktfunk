// Error boundary — contains ANY render failure in our UI so a single bad render can never take
// down the whole Quick Access "Decky" section (Decky's tab-level boundary shows the generic
// "Something went wrong while displaying this content" for the entire tab when one plugin
// throws). The realistic trigger is a future Steam client update that makes a @decky/ui
// component resolve to `undefined` (React then throws "Element type is invalid"). The fallback
// is built from ONLY plain DOM elements + inline styles, so it cannot itself depend on a
// (possibly broken) Steam-internal component — it is guaranteed to render.
import { Component, ErrorInfo, ReactNode } from "react";
import { t } from "../i18n";

export class PluginErrorBoundary extends Component<
  { children: ReactNode },
  { error: Error | null }
> {
  state: { error: Error | null } = { error: null };

  static getDerivedStateFromError(error: Error) {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    // Surface it for diagnosis, but never rethrow — containment is the whole point.
    // eslint-disable-next-line no-console
    console.error("[punktfunk] contained UI render error:", error, info?.componentStack);
  }

  render() {
    const { error } = this.state;
    if (!error) return this.props.children;
    return (
      <div style={{ padding: "1em", lineHeight: 1.45 }}>
        <div style={{ fontWeight: "bold", marginBottom: "0.4em" }}>
          {t("decky_boundary_title")}
        </div>
        <div style={{ opacity: 0.8, marginBottom: "0.6em" }}>
          {t("decky_boundary_body")}
        </div>
        <div
          style={{
            opacity: 0.55,
            fontFamily: "monospace",
            fontSize: "0.8em",
            wordBreak: "break-word",
          }}
        >
          {String(error?.message ?? error)}
        </div>
      </div>
    );
  }
}
