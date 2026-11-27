export type DisplayInfo = {
  id: number;
  widthPoints: number;
  heightPoints: number;
  pointPixelScale?: number | null;
  sourcePixels?: PixelSize | null;
  isMain: boolean;
  displayRef: string;
  snapshotToken: string;
};

export type CaptureRegion = { x: number; y: number; width: number; height: number; origin: "top-left"; unit: "display-logical" };
export type CaptureArea = { mode: "display" | "region"; displayRef: string; snapshotToken: string; region: CaptureRegion | null };
export type TemporaryTargetTicket = { targetId: string; revision: number };
export type TemporaryCaptureTarget = TemporaryTargetTicket & {
  requestId: string;
  displayId: number;
  area: CaptureArea;
  display: DisplayInfo;
};
export type TemporaryTargetSnapshot = {
  revision: number;
  phase: string;
  pending: TemporaryCaptureTarget | null;
  validationScope: "logical-cached-snapshot";
};
export type RegionSelectionResponse = { protocolVersion: 2; requestId: string; status: "confirmed" | "cancelled" | "invalidated" | "failed"; area: CaptureArea | null; raw: string | null };
export type PixelSize = { width: number; height: number };
export type CaptureGeometry = { sourcePixels: PixelSize; outputPixels: PixelSize; paddingRight:number; paddingBottom:number; pointPixelScale:number };

export type CaptureStatus = {
  state: "idle" | "starting" | "recording" | "finalizing" | "failed";
  startedAtUnixMs: number | null;
  workingPath: string | null;
  finalPath: string | null;
  message: AppMessage | null;
};

export type StartCaptureRequest = {
  outputDirectory: string;
  subfolder: string;
  fileName: string;
  displayId: number;
  systemAudio: boolean;
  microphone: boolean;
  maxDurationSeconds: number;
  area?: CaptureArea;
  temporaryTarget?: TemporaryTargetTicket;
  approvedOutput?: {width:number;height:number};
};

export type StopCaptureResult = {
  recordedAtUnixMs?: number;
  historyWarnings: AppMessage[];
  status: CaptureStatus;
  validated: boolean;
  durationSeconds: number | null;
  videoStreamCount: number;
  audioStreamCount: number;
  manifestPath: string;
  captureGeometry?: CaptureGeometry | null;
};

export type PostProcessRequest = {
  manifestPath: string;
  trimTrailingSilence: boolean;
  splitLongVideo: boolean;
  splitThresholdMinutes: number;
  splitSegmentMinutes: number;
  transcribe: boolean;
  transcriptionLocale: string;
};

export type TranscriptionInfo = { requestedLocale: string; usedLocales: string[] };
export type TranscriptionLanguages = {
  available: boolean;
  supportedLocales: string[];
  installedLocales: string[];
  choices: Array<{ locale: string; resolvedLocale?: string; supported: boolean; installed: boolean; modelState: string }>;
};

export type PostProcessProgress = {
  message: AppMessage;
};

export type ProcessingSnapshot = {
  sessionId: string; revision: number; requestId: string | null; runId: string | null;
  state: string; admissionBlocked: boolean;
  active: boolean; stopping: boolean; request: PostProcessRequest | null;
  message: AppMessage | null; result: PostProcessResult | null;
};
export type ExitSnapshot = {
  revision: number; phase: "idle" | "requested" | "confirming" | "stopping-processing" | "stopping-capture" | "issue" | "allowed";
  message: AppMessage | null; captureFailure: CaptureFailureView | null; runId: string | null;
  requestId?: string | null;
};

export type WorkerArtifact = {
  role: "user_output" | "transcript" | string;
  path: string;
  state: string;
  sourceTimelineOffsetSeconds: number;
};

export type PostProcessResult = {
  run?: ProcessingRunView | null;
  error?: AppMessage | null;
  historyWarnings: AppMessage[];
  status: "completed" | "partial" | "failed" | "not-executed" | "unknown" | "cancelled";
  manifestPath: string;
  sourceMaster: string;
  sourceDurationSeconds: number;
  videoOutputs: string[];
  // videoPath identifies the recognition input: the full Source Master, not a video part.
  transcripts: Array<{ videoPath: string; path: string; recognitionLocale: string }>;
  trim: { status?: string; [key: string]: unknown };
  split: { status?: string; [key: string]: unknown };
  artifacts: WorkerArtifact[];
  warnings: AppMessage[];
  elapsedSeconds: number;
  workerLog: string;
  transcription?: TranscriptionInfo | null;
};

export type RecordingHistoryItem = {
  recordId: string;
  jobId: string;
  recordedAtUnixMs: number | null;
  lastConfirmedUnixMs: number;
  manifestState: string;
  snapshotStale: boolean;
  processingAvailable: boolean;
  artifacts: Array<{ role: string; path: string; physicalIdentity: string; availability: string }>;
  manifestPath: string;
  name: string;
  sourceMasterPath: string;
  sourceMasterAvailable: boolean;
  durationSeconds: number;
  processingStatus: string | null;
  processingRuns?: ProcessingRun[];
  transcription?: TranscriptionInfo | null;
  warningCount: number;
  warnings: AppMessage[];
  transcriptCount: number;
  userOutputCount: number;
  modifiedAtUnixMs: number;
};

export type RecordingHistoryResult = {
  items: RecordingHistoryItem[];
  warnings: AppMessage[];
  captureFailures?: CaptureFailureView[];
  processingRunUpdates?: ProcessingRunView[];
};

export type HistoryWindowSnapshot = {
  sessionId: string; revision: number; selectionRevision: number;
  selectedIds: string[]; selectionExplicit: boolean; language: "ja" | "en";
  catalog: RecordingHistoryResult | null; focusProcessRevision: number;
};

export type ProcessingSettings = Omit<PostProcessRequest, "manifestPath" | "transcriptionLocale"> & { transcriptionLocale: string | null };
export type ProcessingRun = {
  runId?: string; runSchemaVersion?: number; recordId?: string; jobId?: string; manifestPath?: string;
  status: string; acceptedAtUnixMs?: number; startedAtUnixMs?: number; finishedAtUnixMs?: number;
  settings?: ProcessingSettings; error?: AppMessage; warnings: AppMessage[]; artifactPaths: string[];
  transcription?: TranscriptionInfo | null;
  publishedArtifacts?: Array<{role:string;path:string;validation:string;fileIdentity:string;sourceTimelineOffsetSeconds:number|null}>;
};
export type ProcessingRunView = {
  run: ProcessingRun; manifestPath: string; manifestApplied: boolean; outboxSaved: boolean; active: boolean; recordError: AppMessage | null;
  artifacts: Array<{path:string;role:string;availability:string}>;
};

export type FileObservation = { path: string; state: "present" | "missing" | "unknown" | "unavailable"; checkedAtUnixMs: number };
export type CaptureSaveFailure = {
  format: "capturescribe.capture-save-failure";
  schemaVersion: 1;
  attemptId: string;
  jobId: string;
  recordingName: string;
  recordedAtUnixMs: number;
  failedAtUnixMs: number;
  outcome: "save_failed";
  error: AppMessage;
  plannedFinalFile: FileObservation;
  workingFile: FileObservation;
};
export type CaptureFailureView = {
  record: CaptureSaveFailure;
  persisted: boolean;
  persistenceError: AppMessage | null;
  plannedFinalFile: FileObservation;
  workingFile: FileObservation;
};
export type AppMessage = {
  code?: string;
  params?: Record<string, string | number>;
  raw?: string;
};
