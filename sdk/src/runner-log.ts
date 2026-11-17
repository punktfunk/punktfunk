// The runner's line sink, shared by discovery, the unit-file trust check and supervision.

/**
 * Severity of a runner line. Only three, because that is all the runner distinguishes: it is
 * reporting on units, not producing application logs.
 */
export type RunnerLogLevel = "info" | "warn" | "error";

/** The sink shape used internally; `defaultLog` reads an omitted level as `info`. */
export type LogSink = (line: string, level?: RunnerLogLevel) => void;

/** Stamped stdout, with `warn`/`error` going to the matching console method (hence stderr). */
export const defaultLog: LogSink = (line, level = "info") => {
	const stamped = `${new Date().toISOString()} ${line}`;
	if (level === "error") console.error(stamped);
	else if (level === "warn") console.warn(stamped);
	else console.log(stamped);
};
