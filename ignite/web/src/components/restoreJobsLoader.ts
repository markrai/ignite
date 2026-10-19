// Slice 1G-A: loading the administrator's restore jobs (read-only). Kept free of React so the
// refresh rules are testable on their own:
// * never two requests at once (a refresh while one is in flight joins it);
// * nothing is applied after `dispose` (the section unmounted);
// * a list requested before newer data arrived (`upsert`) is discarded, never applied over it;
// * a transient failure keeps the last list (marked with the error); a refused session or
//   access drops it, so a revoked administrator does not keep seeing privileged data.

import { ControlError, type RestoreJob } from "../api/controlApi";

export type RestoreErrorKind =
  /** 401: not (or no longer) signed in. */
  | "session"
  /** 403 `loopback_required`: restore administration needs the server's own address or a tunnel. */
  | "loopback"
  /** Any other refusal (403). */
  | "access"
  /** The server answered something this version does not understand. */
  | "unsupported"
  /** Unreachable or a server error: worth retrying, the last list stays visible. */
  | "transient";

export interface RestoreLoadError {
  kind: RestoreErrorKind;
  message: string;
}

export interface RestoreJobsState {
  /** `null` until the first successful load, and again after access was refused. */
  jobs: RestoreJob[] | null;
  loading: boolean;
  error: RestoreLoadError | null;
}

export const LOOPBACK_EXPLANATION =
  "Restore administration is only available when Ignite is opened on the server itself (its local loopback address) or through a secure tunnel to it, such as SSH port forwarding. Access from other devices on the network is refused by design.";

export function describeRestoreError(e: unknown): RestoreLoadError {
  if (!(e instanceof ControlError)) return { kind: "transient", message: "Could not load restore jobs." };
  if (e.code === "unsupported_response") return { kind: "unsupported", message: e.message };
  if (e.status === 401) return { kind: "session", message: "Your administrator session is no longer valid. Sign in again to manage restores." };
  if (e.status === 403 && e.code === "loopback_required") return { kind: "loopback", message: LOOPBACK_EXPLANATION };
  if (e.status === 403) return { kind: "access", message: e.message || "Restore administration was refused." };
  return { kind: "transient", message: e.message || "Could not load restore jobs." };
}

export class RestoreJobsLoader {
  private state: RestoreJobsState = { jobs: null, loading: false, error: null };
  private inFlight: Promise<void> | null = null;
  private disposed = false;
  /** Requests are numbered; answers to requests up to `staleThrough` are discarded. */
  private seq = 0;
  private staleThrough = 0;
  /**
   * Counts successful, non-stale server answers only: evidence of the server's current state.
   * Failed or discarded requests and local changes (`upsert`, `refuse`) never advance it.
   */
  private serverLists = 0;

  constructor(
    private readonly load: () => Promise<RestoreJob[]>,
    private readonly onChange: (s: RestoreJobsState) => void,
  ) {}

  get current(): RestoreJobsState {
    return this.state;
  }

  /** Is a restore running (worth refreshing automatically)? */
  get executing(): boolean {
    return this.state.jobs?.some((j) => j.state === "executing") ?? false;
  }

  refresh(): Promise<void> {
    if (this.disposed) return Promise.resolve();
    if (this.inFlight) return this.inFlight;
    this.set({ ...this.state, loading: true });
    const mine = ++this.seq;
    const stale = () => mine <= this.staleThrough;
    const run = this.load().then(
      (jobs) => {
        if (stale()) return this.set({ ...this.state, loading: false });
        this.serverLists++;
        this.set({ jobs, loading: false, error: null });
      },
      (e: unknown) => {
        if (stale()) return this.set({ ...this.state, loading: false });
        // Shown (a kept list is marked with the error), but no evidence of the server's state.
        const error = describeRestoreError(e);
        this.set({ jobs: error.kind === "transient" ? this.state.jobs : null, loading: false, error });
      },
    );
    this.inFlight = run.finally(() => {
      this.inFlight = null;
    });
    return this.inFlight;
  }

  /** A fresh list that is not overtaken by a request already in flight. */
  refreshAfter(): Promise<void> {
    return this.inFlight ? this.inFlight.then(() => this.refresh()) : this.refresh();
  }

  /**
   * Show a job the server just returned (a verified upload) before the next list arrives. Any
   * list requested earlier is outdated: it is discarded when it answers.
   */
  upsert(job: RestoreJob): void {
    this.staleThrough = this.seq;
    const others = (this.state.jobs ?? []).filter((j) => j.job_id !== job.job_id);
    this.set({ ...this.state, jobs: [job, ...others], error: null });
  }

  /**
   * An action was refused for lack of access: drop the list now, show why, and discard every
   * request already in flight (an earlier successful answer must not bring the list back). A
   * later refresh that the server answers again restores it as usual.
   */
  refuse(error: RestoreLoadError): void {
    this.staleThrough = this.seq;
    this.set({ jobs: null, loading: false, error });
  }

  /**
   * How many successful server lists have been applied: a list shown at a higher version was
   * answered by the server after one shown at a lower version. Failures, discarded answers and
   * local changes never advance it, so it can prove what the server reported.
   */
  get serverVersion(): number {
    return this.serverLists;
  }

  /**
   * Something just changed on the server (e.g. a restore was accepted): every request already
   * in flight is outdated and discarded. Returns the server version that is now outdated (only
   * a later successful answer has a higher one).
   */
  invalidate(): number {
    this.staleThrough = this.seq;
    return this.serverLists;
  }

  dispose(): void {
    this.disposed = true;
  }

  private set(s: RestoreJobsState): void {
    if (this.disposed) return;
    this.state = s;
    this.onChange(s);
  }
}
