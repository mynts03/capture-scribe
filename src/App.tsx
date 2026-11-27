import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { readText } from "@tauri-apps/plugin-clipboard-manager";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import {
  detectPreferredLocale,
  isLocale,
  toAppMessage,
  translate,
  type Locale,
  type TranslationKey,
} from "./i18n";
import { MessageView } from "./MessageView";
import { AppExitDialog } from "./AppExitDialog";
import { newerProcessingSnapshot } from "./executionState";
import { CaptureFailureView } from "./CaptureFailureView";
import { ProcessingRunsView, reconcileProcessResult } from "./ProcessingRunsView";
import { mergeCaptureFailures, latestCaptureFailure } from "./captureFailureSelection";
import { restoreCaptureMode, restoreCaptureArea, areaMatchesDisplay, captureTargetPixels, confirmedSelectionFromResponse } from "./captureSelection";
import { chooseDetectedDisplay, createDisplayDiscovery } from "./displayDiscovery";
import { acceptTemporaryTargetSnapshot, resolveCaptureTarget, temporaryTargetTicket } from "./temporaryCaptureTarget";
import { restoreTranscriptionLanguage, resolveTranscriptionLocale, recognitionLocales, type RecognitionLocale, type TranscriptionLanguage } from "./transcriptionLanguage";
import type { TranscriptionLanguages } from "./types";
import { getLatestRecording, getProcessingTarget } from "./recordingSelection";
import { navigateToCard } from "./cardNavigation";
import { CardNavigation } from "./NavigationBar";
import { useHistoryWindowState } from "./historyWindowState";
import { restoreSplitSettings, parseSplitMinutes, changeSplitSegment, splitDraft, changeSplitDraft, linkSplitDraft, applySplitDraft, MAX_SPLIT_MINUTES, type SplitDraft, type SplitSettings } from "./splitSettings";
import { fileNameProblem, previewFilePath, restoreDestination, shouldSwitchDestination, type SavedDestination } from "./saveSettings";
import type {
  AppMessage,
  CaptureStatus,
  DisplayInfo,
  PostProcessRequest,
  ProcessingSnapshot,
  ExitSnapshot,
  PostProcessResult,
  RecordingHistoryItem,
  RecordingHistoryResult,
  HistoryWindowSnapshot,
  CaptureFailureView as CaptureFailure,
  ProcessingRunView,
  StartCaptureRequest,
  StopCaptureResult,
  CaptureArea,
  RegionSelectionResponse,
  TemporaryCaptureTarget,
  TemporaryTargetSnapshot,
} from "./types";

const idleStatus: CaptureStatus = {
  state: "idle",
  startedAtUnixMs: null,
  workingPath: null,
  finalPath: null,
  message: null,
};

const settingsKey = "capture-scribe-settings-v1";

type SavedSettings = Partial<SplitSettings & TranscriptionLanguage> & {
  captureMode?: "display" | "region";
  captureArea?: CaptureArea | null;
  language?: Locale;
  outputDirectory?: string;
  subfolder?: string;
  systemAudio?: boolean;
  microphone?: boolean;
  maxDurationMinutes?: number;
  trimTrailingSilence?: boolean;
  splitLongVideo?: boolean;
  transcribe?: boolean;
};

const statusTranslationKeys: Record<CaptureStatus["state"] | "processing", TranslationKey> = {
  idle: "statusIdle",
  starting: "statusStarting",
  recording: "statusRecording",
  finalizing: "statusFinalizing",
  failed: "statusFailed",
  processing: "statusProcessing",
};

const processStatusTranslationKeys = {
  completed: "processStatusCompleted",
  partial: "processStatusPartial",
  failed: "processStatusFailed",
  "not-executed": "runNotExecuted",
  unknown: "runUnknown",
  cancelled: "runCancelled",
} as const satisfies Record<PostProcessResult["status"], TranslationKey>;

function formatElapsed(seconds: number) {
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const remainder = seconds % 60;
  return [hours, minutes, remainder]
    .map((value) => value.toString().padStart(2, "0"))
    .join(":");
}

function CaptureTargetDetail({ display, area = null, language }: { display: DisplayInfo; area?: CaptureArea | null; language: Locale }) {
  const pixels = captureTargetPixels(display, area);
  return <p className="capture-target-detail">
    <strong>{pixels ? translate(language, "captureTargetPixelSize", pixels) : translate(language, "captureTargetPixelsUnavailable")}</strong>
    <small>{area?.region ? translate(language, "regionSummary", {
      x: area.region.x, y: area.region.y, width: area.region.width, height: area.region.height,
    }) : translate(language, "fullDisplaySummary", { width: display.widthPoints, height: display.heightPoints })}</small>
  </p>;
}

export default function App() {
  const [language, setLanguage] = useState<Locale>(detectPreferredLocale);
  const [displays, setDisplays] = useState<DisplayInfo[]>([]);
  const [displayId, setDisplayId] = useState<number | null>(null);
  const [displayState, setDisplayState] = useState<"idle" | "loading" | "ready" | "empty" | "missing" | "failed">("idle");
  const [displayError, setDisplayError] = useState<AppMessage | null>(null);
  const displayPreference = useRef<string | null>(null);
  const [displayDiscovery] = useState(() => createDisplayDiscovery(() => invoke<DisplayInfo[]>("list_displays")));
  const [captureMode, setCaptureMode] = useState<"display" | "region">(() => restoreCaptureMode());
  const [captureArea, setCaptureArea] = useState<CaptureArea | null>(null);
  const [selectingRegion, setSelectingRegion] = useState(false);
  // Rust owns temporary targets. None of this state enters SavedSettings.
  const [temporaryTargetSnapshot, setTemporaryTargetSnapshot] = useState<TemporaryTargetSnapshot | null>(null);
  const temporaryTargetRef = useRef<TemporaryTargetSnapshot | null>(null);
  const targetRevision = useRef(0);
  const targetRefreshSequence = useRef(0);
  const [targetSyncReady, setTargetSyncReady] = useState(false);
  const [targetSyncFailed, setTargetSyncFailed] = useState(false);
  const [clearingTarget, setClearingTarget] = useState(false);
  const [captureStartPending, setCaptureStartPending] = useState(false);
  const [activeTemporaryTarget, setActiveTemporaryTarget] = useState<TemporaryCaptureTarget | null>(null);
  const pendingTemporaryTarget = temporaryTargetSnapshot?.pending ?? null;
  const displayedTemporaryTarget = activeTemporaryTarget ?? pendingTemporaryTarget;
  const [outputChoice, setOutputChoice] = useState<{request:StartCaptureRequest;width:number;height:number;alternative:{width:number;height:number}} | null>(null);
  const outputDialog = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    if (outputChoice && !outputDialog.current?.open) outputDialog.current?.showModal();
    if (!outputChoice && outputDialog.current?.open) outputDialog.current.close();
  }, [outputChoice]);
  const handledCaptureResult = useRef<string | null>(null);
  const [outputDirectory, setOutputDirectory] = useState("");
  const [subfolder, setSubfolder] = useState("");
  const persistedDestination = useRef<SavedDestination>({ outputDirectory: "", subfolder: "" });
  const [directoryCheck, setDirectoryCheck] = useState<{
    key: string; value?: { directory: string; exists: boolean }; error?: AppMessage;
  } | null>(null);
  const [pasting, setPasting] = useState(false);
  const nameRequestId = useRef(0);
  const [fileName, setFileName] = useState("CaptureScribe Recording");
  const [systemAudio, setSystemAudio] = useState(true);
  const [microphone, setMicrophone] = useState(false);
  const [maxDurationMinutes, setMaxDurationMinutes] = useState(120);
  const [status, setStatus] = useState<CaptureStatus>(idleStatus);
  const [elapsed, setElapsed] = useState(0);
  const [error, setError] = useState<AppMessage | null>(null);
  const captureErrorRef = useRef<AppMessage | null>(null);
  const [lastResult, setLastResult] = useState<StopCaptureResult | null>(null);
  const [lastFailure, setLastFailure] = useState<CaptureFailure | null>(null);
  const [savedFailures, setSavedFailures] = useState<CaptureFailure[]>([]);
  const [trimTrailingSilence, setTrimTrailingSilence] = useState(false);
  const [splitSettings, setSplitSettings] = useState(() => restoreSplitSettings({}));
  const { splitLongVideo, splitThresholdMinutes, splitSegmentMinutes, splitThresholdLinked } = splitSettings;
  const [segmentDraft, setSegmentDraft] = useState("10");
  const [splitDetails, setSplitDetails] = useState<SplitDraft | null>(null);
  const splitDialog = useRef<HTMLDialogElement>(null);
  const [transcribe, setTranscribe] = useState(true);
  const [transcriptionLanguage, setTranscriptionLanguage] = useState(() => restoreTranscriptionLanguage({}));
  const [transcriptionLanguages, setTranscriptionLanguages] = useState<TranscriptionLanguages | null>(null);
  const [transcriptionLanguageError, setTranscriptionLanguageError] = useState<AppMessage | null>(null);
  const [activeRecognitionLocale, setActiveRecognitionLocale] = useState<string | null>(null);
  const [processing, setProcessing] = useState(false);
  const [executionSnapshot,setExecutionSnapshot]=useState<ProcessingSnapshot|null>(null);
  const executionRef=useRef<ProcessingSnapshot|null>(null);
  const [exitState,setExitState]=useState<ExitSnapshot|null>(null);
  const exitRef=useRef<ExitSnapshot|null>(null);
  const [processingMessage, setProcessingMessage] = useState<AppMessage>({ code: "processing.preparing" });
  const [processResult, setProcessResult] = useState<PostProcessResult | null>(null);
  const [processingRunUpdates, setProcessingRunUpdates] = useState<ProcessingRunView[]>([]);
  const [settingsReady, setSettingsReady] = useState(false);
  const [history, setHistory] = useState<RecordingHistoryItem[]>([]);
  const [historyWarnings, setHistoryWarnings] = useState<AppMessage[]>([]);
  const [historyLoading, setHistoryLoading] = useState(false);
  const { snapshot: sharedHistory, current: sharedHistoryRef, receive: receiveHistory } = useHistoryWindowState(setError);
  const focusedProcessRevision = useRef(0);
  const [defaultHistoryTargetId, setDefaultHistoryTargetId] = useState<string | null>(null);
  const historyRequestId = useRef(0);
  const historyDirectory = useRef<string | null>(null);
  const headerRef = useRef<HTMLElement>(null);

  useLayoutEffect(() => {
    const header = headerRef.current;
    if (!header) return;
    const updateHeight = () => document.documentElement.style.setProperty(
      "--app-header-height", `${header.getBoundingClientRect().height}px`,
    );
    updateHeight();
    // Language/width changes can wrap the header; use its actual height.
    const observer = new ResizeObserver(updateHeight);
    observer.observe(header);
    return () => {
      observer.disconnect();
      document.documentElement.style.removeProperty("--app-header-height");
    };
  }, []);

  const captureBusy = status.state === "starting" || status.state === "recording" || status.state === "finalizing";
  const quitting = !!exitState && exitState.phase!=="idle" && exitState.phase!=="allowed";
  const busy = processing || captureBusy || captureStartPending || quitting || !!executionSnapshot?.admissionBlocked;
  const processingSettingsBusy = captureBusy || quitting;
  const visibleState = processing ? "processing" : status.state;
  const text = (key: TranslationKey, params?: Record<string, string | number>) => translate(language, key, params);
  const latestRecording = getLatestRecording(lastResult, history);
  const failures = mergeCaptureFailures(savedFailures, lastFailure);
  const latestFailure = latestCaptureFailure(failures, history, lastResult);
  const initialHistoryTarget = history.find(item => item.recordId === defaultHistoryTargetId);
  const selectedIds = sharedHistory?.selectedIds ?? [];
  const selectedHistory = selectedIds.length === 1 ? history.find(item => item.recordId === selectedIds[0]) ?? null : null;
  const processingTarget = sharedHistory?.selectionExplicit
    ? selectedHistory
    : getProcessingTarget(null, latestFailure ? null : lastResult?.validated ? latestRecording : initialHistoryTarget ?? latestRecording);
  const multipleTargets = selectedIds.length > 1;
  const destinationKey = JSON.stringify([outputDirectory, subfolder]);
  const resolvedDirectory = directoryCheck?.key === destinationKey ? directoryCheck.value : undefined;
  const directoryError = directoryCheck?.key === destinationKey ? directoryCheck.error : undefined;
  const nameProblem = fileNameProblem(fileName);
  const splitInputInvalid = parseSplitMinutes(segmentDraft) === null;
  const resolvedRecognitionLocale = resolveTranscriptionLocale(transcriptionLanguage, language);
  const recognitionChoice = transcriptionLanguages?.choices.find(choice => choice.locale === resolvedRecognitionLocale);
  const recognitionUnavailable = transcribe && transcriptionLanguages !== null && (!transcriptionLanguages.available || !recognitionChoice?.supported);

  function receiveTemporaryTarget(snapshot: TemporaryTargetSnapshot) {
    if (!acceptTemporaryTargetSnapshot(temporaryTargetRef.current, snapshot, targetRevision.current)) return;
    targetRevision.current = snapshot.revision;
    temporaryTargetRef.current = snapshot;
    setTemporaryTargetSnapshot(snapshot);
    setTargetSyncReady(true); setTargetSyncFailed(false);
  }
  async function refreshTemporaryTarget() {
    const sequence = ++targetRefreshSequence.current;
    try { receiveTemporaryTarget(await invoke<TemporaryTargetSnapshot>("temporary_capture_target_status")); }
    catch { if (sequence === targetRefreshSequence.current) { setTargetSyncReady(false); setTargetSyncFailed(true); } }
  }
  useEffect(() => {
    let disposed = false;
    const refresh = () => { if (!disposed) void refreshTemporaryTarget(); };
    // Subscribe first, then query. Focus/visibility also recover missed events
    // when a WebView was hidden or its listener was temporarily unavailable.
    const unlisten = listen<{ revision: number }>("temporary-capture-target-changed", event => {
      if (disposed || !Number.isSafeInteger(event.payload.revision) || event.payload.revision < targetRevision.current) return;
      targetRevision.current = event.payload.revision;
      setTargetSyncReady(false); refresh();
    }).then(dispose => { if (disposed) { dispose(); return () => {}; } refresh(); return dispose; });
    const visible = () => { if (document.visibilityState === "visible") refresh(); };
    window.addEventListener("focus", refresh);
    document.addEventListener("visibilitychange", visible);
    return () => { disposed = true; void unlisten.then(dispose => dispose()); window.removeEventListener("focus", refresh); document.removeEventListener("visibilitychange", visible); };
  }, []);

  async function clearTemporaryTarget() {
    const target = temporaryTargetRef.current?.pending;
    if (!target || busy || clearingTarget) return;
    setClearingTarget(true); setError(null);
    try { receiveTemporaryTarget(await invoke<TemporaryTargetSnapshot>("clear_temporary_capture_target", { ticket: temporaryTargetTicket(target) })); }
    catch (reason) { setError(toAppMessage(reason)); }
    finally { await refreshTemporaryTarget(); setClearingTarget(false); }
  }

  async function refreshTranscriptionLanguages() {
    try {
      setTranscriptionLanguages(await invoke<TranscriptionLanguages>("transcription_languages"));
      setTranscriptionLanguageError(null);
    } catch (reason) { setTranscriptionLanguageError(toAppMessage(reason)); }
  }
  useEffect(() => { void refreshTranscriptionLanguages(); }, []);

  useEffect(() => {
    if (splitDetails !== null && !splitDialog.current?.open) splitDialog.current?.showModal();
    if (splitDetails === null && splitDialog.current?.open) splitDialog.current.close();
  }, [splitDetails]);

  useEffect(() => {
    void Promise.all([
      invoke<string>("default_output_directory"),
      invoke<CaptureStatus>("capture_status"),
    ])
      .then(async ([directory, current]) => {
        let saved: SavedSettings = {};
        try {
          const value: unknown = JSON.parse(window.localStorage.getItem(settingsKey) ?? "{}");
          if (value && typeof value === "object" && !Array.isArray(value)) saved = value as SavedSettings;
        } catch {
          window.localStorage.removeItem(settingsKey);
        }
        const restoredLanguage = isLocale(saved.language) ? saved.language : detectPreferredLocale();
        setLanguage(restoredLanguage);
        receiveHistory(await invoke<HistoryWindowSnapshot>("set_history_language", { language: restoredLanguage }));
        setCaptureMode(restoreCaptureMode(saved.captureMode));
        const restoredArea = restoreCaptureArea(saved.captureArea);
        setCaptureArea(restoredArea);
        displayPreference.current = restoredArea?.displayRef ?? null;
        const destination = restoreDestination(saved, directory);
        persistedDestination.current = destination;
        setOutputDirectory(destination.outputDirectory);
        setSubfolder(destination.subfolder);
        if (typeof saved.systemAudio === "boolean") setSystemAudio(saved.systemAudio);
        if (typeof saved.microphone === "boolean") setMicrophone(saved.microphone);
        if (typeof saved.maxDurationMinutes === "number" && Number.isFinite(saved.maxDurationMinutes)) {
          setMaxDurationMinutes(Math.min(1440, Math.max(1, saved.maxDurationMinutes)));
        }
        if (typeof saved.trimTrailingSilence === "boolean") setTrimTrailingSilence(saved.trimTrailingSilence);
        const restoredSplit = restoreSplitSettings(saved);
        setSplitSettings(restoredSplit);
        setSegmentDraft(String(restoredSplit.splitSegmentMinutes));
        if (typeof saved.transcribe === "boolean") setTranscribe(saved.transcribe);
        setTranscriptionLanguage(restoreTranscriptionLanguage(saved));
        setStatus(current);
        setSettingsReady(true);
      })
      .catch((reason) => setError(toAppMessage(reason)));

    const unlisten = listen<CaptureStatus>("capture-status", (event) => {
      setStatus(event.payload);
      if (event.payload.state === "idle" || event.payload.state === "failed") setActiveTemporaryTarget(null);
    });
    let disposed=false;
    const receiveProcessing=(snapshot:ProcessingSnapshot)=>{if(!disposed)receiveProcessingSnapshot(snapshot);};
    const receiveExit=(snapshot:ExitSnapshot)=>{if(!disposed&&(!exitRef.current||snapshot.revision>exitRef.current.revision)){exitRef.current=snapshot;setExitState(snapshot);}};
    const unlistenProgress = listen<ProcessingSnapshot>("processing-snapshot",event=>receiveProcessing(event.payload)).then(dispose=>{
      if(disposed){dispose();return ()=>{};}void invoke<ProcessingSnapshot>("processing_snapshot").then(receiveProcessing).catch(reason=>setError(toAppMessage(reason)));return dispose;
    });
    const unlistenExit=listen<ExitSnapshot>("app-exit-state",event=>receiveExit(event.payload)).then(dispose=>{
      if(disposed){dispose();return ()=>{};}void invoke<ExitSnapshot>("app_exit_snapshot").then(receiveExit).catch(reason=>setError(toAppMessage(reason)));return dispose;
    });
    const unlistenComplete = listen<StopCaptureResult>("capture-completed", event => receiveCaptureResult(event.payload)).then(dispose => {
      // Register first, then read the last result: no missed completion gap
      // when the WebView is recreated during finalization.
      void invoke<StopCaptureResult | null>("last_capture_result").then(result => { if (result) receiveCaptureResult(result); }).catch(reason => setError(toAppMessage(reason)));
      return dispose;
    });
    const unlistenFailure = listen<CaptureFailure>("capture-save-failed", event => receiveCaptureFailure(event.payload)).then(dispose => {
      void invoke<CaptureFailure | null>("last_capture_failure").then(failure => { if (failure) receiveCaptureFailure(failure); }).catch(reason => setError(toAppMessage(reason)));
      return dispose;
    });
    return () => {
      disposed=true;
      void unlisten.then((dispose) => dispose());
      void unlistenProgress.then((dispose) => dispose());
      void unlistenExit.then(dispose=>dispose());
      void unlistenComplete.then(dispose => dispose());
      void unlistenFailure.then(dispose => dispose());
    };
  }, []);

  useEffect(() => {
    if (!sharedHistory) return;
    if (settingsReady) setLanguage(sharedHistory.language);
    if (sharedHistory.catalog) {
      setHistory(sharedHistory.catalog.items);
      setSavedFailures(sharedHistory.catalog.captureFailures ?? []);
      setProcessingRunUpdates(sharedHistory.catalog.processingRunUpdates ?? []);
      setProcessResult(current => reconcileProcessResult(current, sharedHistory.catalog!));
    }
    if (sharedHistory.focusProcessRevision > focusedProcessRevision.current) {
      focusedProcessRevision.current = sharedHistory.focusProcessRevision;
      navigateToCard(document.getElementById("process-heading"), window.matchMedia("(prefers-reduced-motion: reduce)").matches);
    }
  }, [sharedHistory, settingsReady]);

  async function changeLanguage(value: Locale) {
    setLanguage(value);
    try { receiveHistory(await invoke<HistoryWindowSnapshot>("set_history_language", { language: value })); }
    catch (reason) { setError(toAppMessage(reason)); }
  }
  async function openHistory() {
    try { await invoke("open_recording_history"); } catch (reason) { setError(toAppMessage(reason)); }
  }
  async function selectProcessingRecord(item: RecordingHistoryItem) {
    try { receiveHistory(await invoke<HistoryWindowSnapshot>("set_history_selection", { recordIds: [item.recordId], selected: true, replace: true })); }
    catch (reason) { setError(toAppMessage(reason)); }
  }

  useEffect(() => {
    if (!settingsReady) return;
    // Invalid/pending drafts never replace the last accepted folder settings.
    if (resolvedDirectory) persistedDestination.current = { outputDirectory, subfolder };
    const settings: SavedSettings = {
      captureMode,
      captureArea,
      language,
      ...persistedDestination.current,
      systemAudio,
      microphone,
      maxDurationMinutes,
      trimTrailingSilence,
      ...splitSettings,
      ...transcriptionLanguage,
      transcribe,
    };
    window.localStorage.setItem(settingsKey, JSON.stringify(settings));
  }, [settingsReady, language, outputDirectory, subfolder, resolvedDirectory, systemAudio, microphone, maxDurationMinutes, trimTrailingSilence, splitSettings, transcribe, transcriptionLanguage, captureMode, captureArea]);

  useEffect(() => {
    document.documentElement.lang = language;
  }, [language]);

  useEffect(() => {
    if (!settingsReady) return;
    let cancelled = false;
    const timer = window.setTimeout(() => {
      void invoke<{ directory: string; exists: boolean }>("preview_save_directory", { outputDirectory, subfolder })
        .then(value => { if (!cancelled) setDirectoryCheck({ key: destinationKey, value }); })
        .catch(reason => { if (!cancelled) setDirectoryCheck({ key: destinationKey, error: toAppMessage(reason) }); });
    }, 150);
    return () => { cancelled = true; window.clearTimeout(timer); };
    // Starting/stopping can create the previously missing directory. Refresh
    // its existence so the final-folder Finder action does not stay disabled.
  }, [settingsReady, destinationKey, outputDirectory, subfolder, status.state]);

  useEffect(() => {
    // Restore the global catalogue even if the saved output volume is offline.
    if (settingsReady) void refreshHistory(null);
  }, [settingsReady]);

  useEffect(() => {
    if (!settingsReady || !resolvedDirectory) return;
    if (shouldSwitchDestination(historyDirectory.current, resolvedDirectory.directory)) {
      historyDirectory.current = resolvedDirectory.directory;
    }
    void refreshHistory(resolvedDirectory.directory);
  }, [settingsReady, resolvedDirectory]);

  useEffect(() => {
    if (status.state !== "recording" || status.startedAtUnixMs === null) {
      if (status.state === "idle") setElapsed(0);
      return;
    }
    const update = () => setElapsed(Math.max(0, Math.floor((Date.now() - status.startedAtUnixMs!) / 1000)));
    update();
    const timer = window.setInterval(update, 250);
    return () => window.clearInterval(timer);
  }, [status.state, status.startedAtUnixMs]);

  const selectedDisplay = displays.find(d => d.id === displayId);
  const regionValid = areaMatchesDisplay(captureArea, selectedDisplay);
  const targetForStart = resolveCaptureTarget(pendingTemporaryTarget, displays, selectedDisplay, captureMode, captureArea);
  const temporaryDisplay = activeTemporaryTarget?.display ?? (displayState === "ready" && targetForStart?.temporaryTarget
    ? displays.find(display => display.id === targetForStart.displayId) : undefined);
  const canStart = useMemo(
    () => !busy && !selectingRegion && !pasting && !clearingTarget && targetSyncReady && displayState === "ready" && !!targetForStart && resolvedDirectory !== undefined && nameProblem === null,
    [busy, selectingRegion, pasting, clearingTarget, targetSyncReady, displayState, targetForStart, resolvedDirectory, nameProblem],
  );

  useEffect(() => {
    // Saved area/preferred identity is restored before initial discovery.
    if (settingsReady) void refreshDisplays();
    return () => displayDiscovery.invalidate();
  }, [settingsReady, displayDiscovery]);

  useEffect(() => {
    // Revalidate when returning to the app. Native start/selector validation
    // remains authoritative if the display changes while the app stays focused.
    const refreshOnFocus = () => {
      if (settingsReady && !busy && !selectingRegion) void refreshDisplays();
    };
    window.addEventListener("focus", refreshOnFocus);
    return () => window.removeEventListener("focus", refreshOnFocus);
  }, [settingsReady, busy, selectingRegion, displayDiscovery]);

  async function refreshDisplays() {
    if (!settingsReady || busy || selectingRegion) return;
    setDisplayState("loading"); setDisplayError(null);
    const pending = displayDiscovery.refresh();
    const version = displayDiscovery.version;
    const result = await pending;
    if (!result || version !== displayDiscovery.version) return;
    if (result.status === "failure") {
      setDisplays([]); setDisplayId(null); setDisplayState("failed");
      setDisplayError(toAppMessage(result.reason));
      return;
    }
    const selected = chooseDetectedDisplay(result.displays, displayPreference.current);
    setDisplays(result.displays); setDisplayId(selected?.id ?? null);
    if (selected) displayPreference.current = selected.displayRef;
    setDisplayState(!result.displays.length ? "empty" : selected ? "ready" : "missing");
  }

  async function selectRegion() {
    if (busy || selectingRegion || pendingTemporaryTarget || !targetSyncReady || displayState !== "ready" || !selectedDisplay) return;
    setSelectingRegion(true); setError(null);
    const requestId = crypto.randomUUID();
    try {
      const result = await invoke<RegionSelectionResponse>("select_capture_region", {request: {
        protocolVersion: 2, requestId, initialMode: captureMode, displayId: selectedDisplay.id, displayRef: selectedDisplay.displayRef,
        snapshotToken: selectedDisplay.snapshotToken, initialRegion: regionValid ? captureArea?.region : null, language,
      }});
      const confirmed = confirmedSelectionFromResponse(result, requestId, selectedDisplay);
      if (confirmed) {
        // Full-screen confirmation changes only the mode, never the saved rectangle.
        if (confirmed.mode === "region") setCaptureArea(confirmed);
        setCaptureMode(confirmed.mode);
      }
    } catch (reason) { setError(toAppMessage(reason)); }
    finally { setSelectingRegion(false); }
  }

  function changeDestination(base: string, relative: string) {
    if (base === outputDirectory && relative === subfolder) return;
    setOutputDirectory(base);
    setSubfolder(relative);
  }

  function changeOutputDirectory(value: string) { changeDestination(value, subfolder); }

  async function pasteFileName() {
    const request = ++nameRequestId.current;
    setPasting(true);
    setError(null);
    try {
      const value = await readText().catch(reason => {
        throw { code: "save.clipboard_unavailable", raw: toAppMessage(reason).raw ?? String(reason) };
      });
      // The exact same Rust validator is used by recording start.
      await invoke("validate_file_name", { fileName: value });
      if (request === nameRequestId.current) setFileName(value);
    } catch (reason) {
      if (request === nameRequestId.current) setError(toAppMessage(reason));
    } finally {
      if (request === nameRequestId.current) setPasting(false);
    }
  }

  async function chooseOutputDirectory() {
    setError(null);
    try {
      const selected = await open({
        directory: true,
        multiple: false,
        defaultPath: outputDirectory.trim() || undefined,
        title: text("outputDirectoryDialogTitle"),
      });
      if (typeof selected !== "string") return;

      changeOutputDirectory(selected);
    } catch (reason) {
      setError(toAppMessage(reason));
    }
  }

  async function revealPath(path: string) {
    setError(null);
    try {
      await invoke("reveal_path", { path });
    } catch (reason) {
      setError(toAppMessage(reason));
    }
  }

  async function startCapture() {
    if (!canStart || !targetForStart) return;
    setError(null);
    const request: StartCaptureRequest = {
      outputDirectory,
      subfolder,
      fileName,
      ...targetForStart,
      systemAudio,
      microphone,
      maxDurationSeconds: Math.min(86400, Math.max(60, Math.round((Number.isFinite(maxDurationMinutes) ? maxDurationMinutes : 120) * 60))),
    };
    // Freeze the displayed target before Rust emits its consumed revision.
    setActiveTemporaryTarget(pendingTemporaryTarget);
    await runCaptureRequest(request);
  }

  async function runCaptureRequest(request: StartCaptureRequest) {
    setCaptureStartPending(true);
    try {
      // A delayed response must not restore recording after a short capture ended.
      await invoke<CaptureStatus>("start_capture", { request });
    } catch (reason) {
      const message = toAppMessage(reason);
      captureErrorRef.current = message;
      setError(message);
      setStatus(await invoke<CaptureStatus>("capture_status").catch(() => idleStatus));
      setActiveTemporaryTarget(null);
      // A temporary ticket was consumed by this attempt, even on preparation
      // failure. Never retry that ticket through the output-alternative dialog.
      if (message.code === "capture.output_configuration_failed" && !request.approvedOutput && !request.temporaryTarget) {
        const width=Number(message.params?.width), height=Number(message.params?.height);
        if (Number.isSafeInteger(width) && Number.isSafeInteger(height) && width>0 && height>0) {
          const alternatives=await invoke<Array<{width:number;height:number}>>("capture_output_alternatives",{width,height}).catch(() => []);
          if (alternatives[0]) setOutputChoice({request,width,height,alternative:alternatives[0]});
        }
      }
    } finally {
      if (request.temporaryTarget) await refreshTemporaryTarget();
      setCaptureStartPending(false);
    }
  }

  async function stopCapture() {
    setError(null);
    try {
      const result = await invoke<StopCaptureResult>("stop_capture");
      receiveCaptureResult(result);
    } catch (reason) {
      const message = toAppMessage(reason); captureErrorRef.current = message; setError(message);
      setStatus(await invoke<CaptureStatus>("capture_status").catch(() => idleStatus));
    }
  }

  function receiveCaptureResult(result: StopCaptureResult) {
    setActiveTemporaryTarget(null);
    if (handledCaptureResult.current === result.manifestPath) return;
    handledCaptureResult.current = result.manifestPath;
    setLastResult(result);
    setLastFailure(current => current && current.record.recordedAtUnixMs > (result.recordedAtUnixMs ?? 0) ? current : null);
    void refreshHistory(undefined, result.historyWarnings);
  }

  function receiveCaptureFailure(failure: CaptureFailure) {
    setActiveTemporaryTarget(null);
    setLastFailure(current => !current || failure.record.recordedAtUnixMs >= current.record.recordedAtUnixMs ? failure : current);
    void refreshHistory(undefined, failure.persistenceError ? [failure.persistenceError] : []);
  }

  async function processRecording() {
    if (!sharedHistory || multipleTargets || !processingTarget?.processingAvailable || busy || (splitLongVideo && splitInputInvalid) || recognitionUnavailable) return;
    const manifestPath = processingTarget.manifestPath;
    // A refresh begun before this execution cannot update its result/notices.
    ++historyRequestId.current;
    setHistoryLoading(false);
    setError(null);
    setProcessResult(null);
    setProcessingMessage({ code: "processing.preparing" });
    setProcessing(true);
    const request: PostProcessRequest = {
      manifestPath,
      trimTrailingSilence,
      splitLongVideo,
      splitThresholdMinutes,
      splitSegmentMinutes,
      transcribe,
      transcriptionLocale: resolvedRecognitionLocale,
    };
    setActiveRecognitionLocale(transcribe ? request.transcriptionLocale : null);
    try {
      const snapshot = await invoke<ProcessingSnapshot>("process_recording", { requestId:crypto.randomUUID(), request, selectionRevision: sharedHistoryRef.current?.selectionRevision });
      receiveProcessingSnapshot(snapshot);
    } catch (reason) {
      setError(toAppMessage(reason));
      setProcessing(executionRef.current?.active??false);
      // A lost acceptance reply is not evidence that the app-owned worker
      // stopped. Requery, never resubmit the processing request automatically.
      void invoke<ProcessingSnapshot>("processing_snapshot").then(receiveProcessingSnapshot).catch(()=>{});
    }
  }

  function receiveProcessingSnapshot(snapshot:ProcessingSnapshot){
    if(!newerProcessingSnapshot(executionRef.current,snapshot))return;
    const previous=executionRef.current;executionRef.current=snapshot;setExecutionSnapshot(snapshot);setProcessing(snapshot.active);
    setActiveRecognitionLocale(snapshot.active&&snapshot.request?.transcribe?snapshot.request.transcriptionLocale:null);
    if(snapshot.message)setProcessingMessage(snapshot.message);
    if(snapshot.active&&snapshot.runId&&previous?.runId!==snapshot.runId){++historyRequestId.current;setHistoryLoading(false);setProcessResult(null);void refreshHistory();}
    if(snapshot.result){
      const result=snapshot.result;setProcessResult(result);
      void refreshHistory(undefined, result.historyWarnings, result);
      void refreshTranscriptionLanguages();
    }
  }
  async function stopProcessing(){
    const runId=executionRef.current?.runId;if(!runId)return;
    try{receiveProcessingSnapshot(await invoke<ProcessingSnapshot>("stop_processing",{runId}));}catch(reason){setError(toAppMessage(reason));}
  }
  async function chooseExit(choice:string){
    const revision=exitRef.current?.revision;if(revision===undefined)return;
    try{const state=await invoke<ExitSnapshot>("choose_app_exit",{revision,choice});if(!exitRef.current||state.revision>exitRef.current.revision){exitRef.current=state;setExitState(state);}}
    catch(reason){setError(toAppMessage(reason));}
  }

  async function refreshHistory(directory: string | null = historyDirectory.current, notices: AppMessage[] = [], noticeSource?:PostProcessResult) {
    const selectedDirectory = directory?.trim();
    const requestId = ++historyRequestId.current;
    setHistoryLoading(true);
    try {
      const result = await invoke<RecordingHistoryResult>("list_recording_history", { outputDirectory: selectedDirectory ?? null });
      if (requestId !== historyRequestId.current) return;
      const shared = await invoke<HistoryWindowSnapshot>("history_window_snapshot");
      if (requestId !== historyRequestId.current) return;
      receiveHistory(shared);
      const fresh = sharedHistoryRef.current?.catalog ?? result;
      setProcessResult(current=>reconcileProcessResult(current,fresh));
      setDefaultHistoryTargetId(current => current ?? fresh.items[0]?.recordId ?? null);
      const refreshedNotices=noticeSource?reconcileProcessResult(noticeSource,fresh)?.historyWarnings??notices:notices;
      setHistoryWarnings([...refreshedNotices, ...fresh.warnings]);
      // Recheck paths for an in-memory failure too (its disk write may have
      // failed). Do not trigger receiveCaptureFailure/another refresh here.
      const memoryFailure = await invoke<CaptureFailure | null>("last_capture_failure").catch(() => undefined);
      if (requestId === historyRequestId.current && memoryFailure !== undefined) setLastFailure(memoryFailure);
    } catch (reason) {
      if (requestId === historyRequestId.current) setHistoryWarnings([...notices, toAppMessage(reason)]);
    } finally {
      if (requestId === historyRequestId.current) setHistoryLoading(false);
    }
  }

  async function useLatestForProcessing() {
    await refreshHistory();
    const item = sharedHistoryRef.current?.catalog?.items.find(item => item.manifestPath === latestRecording?.manifestPath);
    if (!item) { setError({ code: "history.selection_changed" }); return; }
    await selectProcessingRecord(item);
    setDefaultHistoryTargetId(history.find(item => item.manifestPath === latestRecording?.manifestPath)?.recordId ?? null);
    setProcessResult(null);
    setError(null);
  }

  return (
    <main className="shell" aria-label="CaptureScribe">
      <h1 className="sr-only">CaptureScribe</h1>
      <header className="app-header" ref={headerRef}>
        <CardNavigation language={language} navigate={headingId => headingId === "results-heading" ? void openHistory() : navigateToCard(document.getElementById(headingId), window.matchMedia("(prefers-reduced-motion: reduce)").matches)} />
        <div className="header-controls">
          <div className="language-switch" role="group" aria-label={text("languageSelector")}>
            <button type="button" className={language === "ja" ? "active" : ""} onClick={() => void changeLanguage("ja")} aria-pressed={language === "ja"}>
              {text("languageJapanese")}
            </button>
            <button type="button" className={language === "en" ? "active" : ""} onClick={() => void changeLanguage("en")} aria-pressed={language === "en"}>
              {text("languageEnglish")}
            </button>
          </div>
          <div className={`status-pill status-${visibleState}`} role="status">
            <span className="status-dot" aria-hidden="true" />
            {text(statusTranslationKeys[visibleState])}
          </div>
        </div>
      </header>

      <p className="app-subtitle">{text("heroSubtitle")}</p>
      {error && <div className="alert" role="alert"><MessageView message={error} language={language} recording={error === captureErrorRef.current} /></div>}

      <div className="studio-top">
        <div className="settings-stack">
          <section className="panel capture-panel" data-purpose="target-and-audio-settings" aria-labelledby="capture-heading">
            <div className="section-heading">
              <h2 id="capture-heading" tabIndex={-1}>{text("navCapture")}</h2>
              <button className="secondary" type="button" onClick={refreshDisplays} disabled={busy || selectingRegion || !settingsReady || displayState === "loading"}>
                {text("loadScreens")}
              </button>
            </div>

            <div className="screen-row">
              <div className="screen-field">
                <label htmlFor="capture-display">
                <span>{text("screenLabel")}</span>
                <select id="capture-display" value={displayId ?? ""} onChange={(event) => {
                  const selected = displays.find(display => display.id === Number(event.target.value));
                  if (!selected) return;
                  displayPreference.current = selected.displayRef;
                  setDisplayId(selected.id); setDisplayState("ready");
                }} disabled={busy || selectingRegion || !!pendingTemporaryTarget || !targetSyncReady || displayState === "loading" || !displays.length}>
                  <option value="" disabled>{text("selectScreenPrompt")}</option>
                  {displays.map((display) => (
                    <option key={display.id} value={display.id}>
                      {display.isMain ? text("mainScreen") : text("screenNumber", { id: display.id })} — {display.widthPoints} × {display.heightPoints} {text("screenPoints")}
                    </option>
                  ))}
                </select>
                </label>
                <small role="status">{text(displayState === "loading" || displayState === "idle" ? "displaysLoading"
                  : displayState === "empty" ? "displaysEmpty" : displayState === "missing" ? "displaySelectionMissing"
                    : displayState === "failed" ? "displaysFailed" : "displaysReady")}</small>
                {displayError && <MessageView message={displayError} language={language} />}
                {displayError?.code === "capture.permission_denied" && <small>{text("displayPermissionHint")}</small>}
              </div>
              <div className="scope-summary">
                <div className="capture-region-edit">
                  <button className="secondary" onClick={selectRegion} disabled={busy || selectingRegion || !!pendingTemporaryTarget || !targetSyncReady || displayState !== "ready" || !selectedDisplay}>{text(selectingRegion ? "selectingRegion" : "configureCaptureRange")}</button>
                </div>
                {!targetSyncReady && <p role="status">{text(targetSyncFailed ? "temporaryTargetSyncFailed" : "temporaryTargetSyncing")}
                  {targetSyncFailed && <button className="secondary" type="button" onClick={() => void refreshTemporaryTarget()}>{text("temporaryTargetRefresh")}</button>}
                </p>}
                {displayedTemporaryTarget ? <div className="temporary-capture-target" role="status" aria-label={text("temporaryTargetTitle")}>
                  <strong>{text(activeTemporaryTarget ? "temporaryTargetActive" : "temporaryTargetTitle")}</strong>
                  <small className="temporary-target-display">{text("temporaryTargetDisplay", {id:displayedTemporaryTarget.displayId})}</small>
                  {temporaryDisplay && <CaptureTargetDetail display={temporaryDisplay} area={displayedTemporaryTarget.area} language={language} />}
                  <small>{text("temporaryTargetNotice")}</small>
                  {!activeTemporaryTarget && <button className="secondary" type="button" onClick={() => void clearTemporaryTarget()} disabled={busy || selectingRegion || clearingTarget || !targetSyncReady}>{text("temporaryTargetClear")}</button>}
                  {!activeTemporaryTarget && targetSyncReady && !targetForStart && <p className="region-required">{text("temporaryTargetInvalid")}</p>}
                </div> : captureMode === "region" && !regionValid
                  ? <p className="region-required" role="status">{text("regionRequired")}</p>
                  : displayState === "ready" && selectedDisplay && <CaptureTargetDetail display={selectedDisplay}
                      area={captureMode === "region" ? captureArea : null} language={language} />}
                {!displayedTemporaryTarget && (temporaryTargetSnapshot?.phase === "consumed" || temporaryTargetSnapshot?.phase === "invalidated") &&
                  <small role="status">{text(temporaryTargetSnapshot.phase === "consumed" ? "temporaryTargetConsumed" : "temporaryTargetInvalidated")}</small>}
                <span className="field-label">{text("captureModeLabel")}</span>
                <div className="capture-scope-buttons" role="group" aria-label={text("captureModeLabel")}>
                  <button className="secondary" aria-pressed={captureMode === "display"} onClick={() => setCaptureMode("display")} disabled={busy || selectingRegion || !!pendingTemporaryTarget || !targetSyncReady}>{text("fullScreen")}</button>
                  <button className="secondary" aria-pressed={captureMode === "region"} onClick={() => setCaptureMode("region")} disabled={busy || selectingRegion || !!pendingTemporaryTarget || !targetSyncReady}>{text("regionMode")}</button>
                </div>
                <small>{text("regionAudioHint")}</small>
              </div>
            </div>

            <div className="toggle-row">
              <label className="toggle-card">
                <input type="checkbox" checked={systemAudio} onChange={(event) => setSystemAudio(event.target.checked)} disabled={busy} />
                <span><strong>{text("systemAudio")}</strong><small>{text("systemAudioDescription")}</small></span>
              </label>
              <label className="toggle-card">
                <input type="checkbox" checked={microphone} onChange={(event) => setMicrophone(event.target.checked)} disabled={busy} />
                <span><strong>{text("microphone")}</strong><small>{text("microphoneDescription")}</small></span>
              </label>
            </div>
          </section>

          <section className="panel save-panel" data-purpose="save-destination-settings" aria-labelledby="save-heading">
            <div className="section-heading">
              <h2 id="save-heading" tabIndex={-1}>{text("navSave")}</h2>
              <label className="duration-field">
                <span>{text("maxDurationMinutes")}</span>
                <input type="number" min={1} max={1440} value={maxDurationMinutes} onChange={(event) => setMaxDurationMinutes(Number(event.target.value))} disabled={busy} />
              </label>
            </div>

            <div>
              <label htmlFor="recording-file-name">{text("fileName")}</label>
              <div className="filename-input-row">
                <input id="recording-file-name" value={fileName} onChange={(event) => {
                  nameRequestId.current += 1; setPasting(false); setFileName(event.target.value);
                }} disabled={busy} aria-invalid={nameProblem !== null} aria-describedby={nameProblem ? "filename-problem" : undefined} />
                <button className="secondary" type="button" onClick={() => void pasteFileName()} disabled={busy || pasting}>{text("pasteFileName")}</button>
              </div>
              {nameProblem && <p className="input-problem" id="filename-problem"><MessageView message={{ code: nameProblem }} language={language} /></p>}
            </div>
            <label className="directory-field">
              <span>{text("baseOutputDirectory")}</span>
              <input
                value={outputDirectory}
                onChange={(event) => changeOutputDirectory(event.target.value)}
                disabled={busy}
                spellCheck={false}
              />
            </label>
            <label className="directory-field">
              <span>{text("subfolder")}</span>
              <input value={subfolder} onChange={event => changeDestination(outputDirectory, event.target.value)} disabled={busy} spellCheck={false} aria-invalid={!!directoryError} aria-describedby="subfolder-hint" placeholder={text("subfolderExample")} />
              <small id="subfolder-hint">{text("subfolderHint")}</small>
            </label>
            <div className="save-preview" aria-live="polite">
              <strong>{text("finalSavePath")}</strong>
              {directoryError ? <MessageView message={directoryError} language={language} />
                : resolvedDirectory ? <>
                    <p>{nameProblem ? resolvedDirectory.directory : previewFilePath(resolvedDirectory.directory, fileName)}</p>
                    {!resolvedDirectory.exists && <small>{text("createFolderOnStart")}</small>}
                  </> : <p>{text("checkingSavePath")}</p>}
              {!resolvedDirectory && <p>{text("saveDraftContextHint")}</p>}
            </div>
            <div className="path-actions">
              <button className="secondary" type="button" onClick={() => void chooseOutputDirectory()} disabled={busy}>
                {text("chooseOutputDirectory")}
              </button>
              <button className="secondary" type="button" onClick={() => resolvedDirectory && void revealPath(resolvedDirectory.directory)} disabled={busy || !resolvedDirectory?.exists}>
                {text("revealOutputDirectory")}
              </button>
            </div>
          </section>
        </div>

        <section className="panel transport-panel" data-purpose="recording-control-panel" aria-labelledby="session-heading">
          <div className="section-heading">
            <h2 id="session-heading" tabIndex={-1}>{text("navSession")}</h2>
            <span className={`transport-state status-${visibleState}`}>
              <span className="status-dot" aria-hidden="true" />{text(statusTranslationKeys[visibleState])}
            </span>
          </div>
          <div className="transport-content">
            <div className="timer" aria-label={text("elapsedTime")}>{formatElapsed(elapsed)}</div>
            <div className="status-message" role="status">
              {status.state === "idle" && !canStart && status.message?.code === "capture.ready"
                ? text("statusFallback")
                : status.message
                ? <MessageView message={status.message} language={language} recording={status.state === "failed"} />
                : text(canStart ? "capture.ready" : "statusFallback")}
            </div>

            {status.state === "recording" ? (
              <button className="record-button stop" type="button" onClick={stopCapture}>
                <span className="stop-icon" aria-hidden="true" />{text("stopRecording")}
              </button>
            ) : (
              <button className="record-button" type="button" onClick={startCapture} disabled={!canStart}>
                <span className="record-icon" aria-hidden="true" />{text("startRecording")}
              </button>
            )}
            {status.finalPath && status.state !== "idle" && (
              <div className="recording-destination"><strong>{text("recordingDestination")}</strong><p>{status.finalPath}</p></div>
            )}
          </div>

          <div className="safety-note">
            <strong>{text("sourceSafetyTitle")}</strong>
            <p>{text("sourceSafetyDescription")}</p>
          </div>
        </section>
      </div>

      <div className="studio-bottom">
        <section className="panel processing-panel" data-purpose="post-processing-options" aria-labelledby="process-heading">
          <div className="section-heading">
            <h2 id="process-heading" tabIndex={-1}>{text("navProcess")}</h2>
            <button className="secondary" type="button" onClick={() => void openHistory()}>{text("chooseRecordings")}</button>
          </div>

          <p className="history-selection">{text("processingSelectionCount", { count: sharedHistory?.selectionExplicit ? selectedIds.length : processingTarget ? 1 : 0 })}</p>
          {multipleTargets && <p className="warning">{text("multipleProcessingDeferred")}</p>}

          <div className="process-state" role="status">
            {processing
              ? <MessageView message={executionSnapshot?.stopping?{code:"processingStopping"}:processingMessage} language={language} />
              : processResult
                ? text(processStatusTranslationKeys[processResult.status])
                : text("processReady")}
          </div>
          {processing&&<>
            <p>{text("nextProcessingSettings")}</p>
            <details className="message-details"><summary>{text("runSettings")}</summary><pre>{JSON.stringify(executionSnapshot?.request,null,2)}</pre></details>
            <button type="button" className="secondary" disabled={quitting||executionSnapshot?.stopping||!executionSnapshot?.runId} onClick={()=>void stopProcessing()}>{text("processingStop")}</button>
            <button type="button" className="secondary" disabled={quitting} onClick={()=>void invoke<ProcessingSnapshot>("processing_snapshot").then(receiveProcessingSnapshot).catch(reason=>setError(toAppMessage(reason)))}>{text("processingRefresh")}</button>
          </>}
          {processingTarget && (
            <p className="history-selection">
              {text("processingTarget")} <b>{processingTarget.name}</b>
              {!processingTarget.processingAvailable && ` · ${text("processingUnavailable")}`}
            </p>
          )}

          <div className="processing-options">
            <label className="toggle-card">
              <input type="checkbox" checked={trimTrailingSilence} onChange={(event) => setTrimTrailingSilence(event.target.checked)} disabled={processingSettingsBusy} />
              <span><strong>{text("trimTrailingSilence")}</strong><small>{text("trimTrailingSilenceDescription")}</small></span>
            </label>
            <label className="toggle-card">
              <input type="checkbox" checked={splitLongVideo} onChange={(event) => setSplitSettings(current => ({ ...current, splitLongVideo: event.target.checked }))} disabled={processingSettingsBusy} />
              <span><strong>{text("splitLongVideo")}</strong><small>{text("splitLongVideoDescription")}</small></span>
            </label>
            <div className="split-settings">
              <div className="split-input-row">
                <label htmlFor="split-segment">{text("splitSegmentMinutes")}
                  <input id="split-segment" type="text" inputMode="numeric" value={segmentDraft} disabled={processingSettingsBusy}
                    aria-invalid={splitInputInvalid} aria-describedby="split-input-help"
                    onChange={event => { setSegmentDraft(event.target.value); setSplitSettings(current => changeSplitSegment(current, event.target.value)); }} />
                </label>
                <button type="button" className="secondary" disabled={processingSettingsBusy} onClick={() => setSplitDetails(splitDraft(splitSettings))}>{text("splitDetails")}</button>
              </div>
              <p className="split-condition">{text(splitLongVideo ? "splitCondition" : "splitConditionOff", { threshold: splitThresholdMinutes, segment: splitSegmentMinutes })} {text(splitThresholdLinked ? "splitLinked" : "splitIndependent")}</p>
              <small id="split-input-help" className={splitInputInvalid ? "input-problem" : ""}>{text(splitInputInvalid ? "processing.split_invalid" : "splitInputHelp", { max: MAX_SPLIT_MINUTES })}</small>
            </div>
            <label className="toggle-card">
              <input type="checkbox" checked={transcribe} onChange={(event) => setTranscribe(event.target.checked)} disabled={processingSettingsBusy} />
              <span><strong>{text("transcribeAudio")}</strong><small>{text("transcribeAudioDescription")}</small></span>
            </label>
            <div className="recognition-settings">
              <label>{text("recognitionMode")}
                <select value={transcriptionLanguage.transcriptionLanguageMode} disabled={processingSettingsBusy}
                  onChange={event => setTranscriptionLanguage(current => ({ ...current, transcriptionLanguageMode: event.target.value === "manual" ? "manual" : "auto" }))}>
                  <option value="auto">{text("recognitionAuto")}</option>
                  <option value="manual">{text("recognitionManual")}</option>
                </select>
              </label>
              <label>{text("recognitionLanguage")}
                <select value={resolvedRecognitionLocale} disabled={processingSettingsBusy || transcriptionLanguage.transcriptionLanguageMode === "auto"}
                  onChange={event => setTranscriptionLanguage(current => ({ ...current, transcriptionManualLocale: event.target.value as RecognitionLocale }))}>
                  {recognitionLocales.map(locale => <option key={locale} value={locale} disabled={!transcriptionLanguages?.available || !transcriptionLanguages.choices.find(choice => choice.locale === locale)?.supported}>
                    {text(locale === "ja-JP" ? "recognitionJapanese" : "recognitionEnglish")}
                  </option>)}
                </select>
              </label>
              <p className="split-help">{text("recognitionAutoHelp")}</p>
              <p className="split-help">{text(activeRecognitionLocale ? "recognitionActive" : "recognitionResolved", { locale: activeRecognitionLocale ?? resolvedRecognitionLocale })}</p>
              {transcriptionLanguageError ? <MessageView message={transcriptionLanguageError} language={language} />
                : transcriptionLanguages === null ? <p className="split-help">{text("recognitionChecking")}</p>
                : !transcriptionLanguages.available ? <MessageView message={{ code: "transcription.unavailable" }} language={language} />
                : !recognitionChoice?.supported ? <MessageView message={{ code: "transcription.locale_unavailable", params: { locale: resolvedRecognitionLocale } }} language={language} />
                : <p className="split-help">{text(recognitionChoice.modelState === "installed" ? "recognitionModelReady" : "recognitionModelPreparing")}</p>}
              <button className="secondary" type="button" disabled={busy} onClick={() => void refreshTranscriptionLanguages()}>{text("recognitionRefresh")}</button>
            </div>
          </div>
          <button className="secondary process-button" type="button" onClick={processRecording}
            disabled={!sharedHistory || multipleTargets || !processingTarget?.processingAvailable || busy || (splitLongVideo && splitInputInvalid) || recognitionUnavailable}>
            {text("processAction")}
          </button>
          {processing && <p className="processing-note">{text("processingAction")}</p>}

          {processResult && (
            <div className={`process-result process-${processResult.status}`}>
              <p className="result-path">{text("processingTarget")} {processResult.sourceMaster}</p>
              <p>
                {processResult.status==="completed"?text("processCompleted"):processResult.status==="partial"?text("processPartial"):text(processStatusTranslationKeys[processResult.status])}
                {` ${processResult.elapsedSeconds.toFixed(1)} ${text("seconds")}`}
              </p>
              {processResult.error && <div className="warning"><MessageView message={processResult.error} language={language}/></div>}
              {processResult.run && (!processResult.run.manifestApplied||!processResult.run.outboxSaved) && <div className="warning"><p>{text(processResult.run.outboxSaved?"runPending":"runUnsaved")}</p>{processResult.run.recordError&&<MessageView message={processResult.run.recordError} language={language}/>}</div>}
              {processResult.run && <ProcessingRunsView runs={[]} updates={[processResult.run]} language={language} busy={busy} reveal={path=>void revealPath(path)}/>}
              {processResult.artifacts.length > 0 ? (
                <ul>
                  {processResult.artifacts.map((artifact) => (
                    <li key={`${artifact.role}-${artifact.path}`}>
                      <b>{artifact.role === "user_output" ? text("artifactUserOutput") : artifact.role === "transcript" ? text("artifactTranscript") : artifact.role}</b>
                      <span className="artifact-path">{artifact.path}</span>
                      <button className="secondary" type="button" disabled={busy||!!processResult.run&&!processResult.run.artifacts.some(a=>a.path===artifact.path&&a.availability==="available")} onClick={() => void revealPath(artifact.path)}>{text("showInFinder")}</button>
                    </li>
                  ))}
                </ul>
              ) : processResult.status === "completed" || processResult.status === "partial" ? <p>{text("noDerivedFiles")}</p> : null}
              {processResult.transcription && <p className="split-help">{text("recognitionUsed", { locales: processResult.transcription.usedLocales.join(", ") || text("recognitionNotUsed") })}</p>}
              {processResult.warnings.map((warning, index) => <div className="warning" key={index}><MessageView message={warning} language={language} /></div>)}
            </div>
          )}
        </section>

        <section className="panel results-panel" data-purpose="recording-history-log" aria-labelledby="results-heading">
          <div className="section-heading">
            <h2 id="results-heading" tabIndex={-1}>{text("resultsHeading")}</h2>
            <button type="button" className="secondary" onClick={() => void openHistory()} disabled={historyLoading}>{text("navResults")}</button>
          </div>

          {(
            latestFailure ? <CaptureFailureView failure={latestFailure} language={language} busy={busy} reveal={path => void revealPath(path)} /> :
            <div className="latest-result" aria-label={text("latestKicker")}>
              <h3>{latestRecording ? text(lastResult?.validated ? "latestValidated" : "latestFromHistory") : text("latestEmpty")}</h3>
              {lastResult?.captureGeometry && <p>{text("capturePixels", {width:lastResult.captureGeometry.outputPixels.width,height:lastResult.captureGeometry.outputPixels.height,right:lastResult.captureGeometry.paddingRight,bottom:lastResult.captureGeometry.paddingBottom})}</p>}
              {latestRecording && <h3>{latestRecording.name}</h3>}
              <p className="result-path">{latestRecording?.sourceMasterPath || text("latestHint")}</p>
              {latestRecording && (
                <>
                  <div className="path-actions">
                    <button className="secondary" type="button" onClick={() => void revealPath(latestRecording.sourceMasterPath)} disabled={busy || !latestRecording.sourceMasterAvailable}>{text("showInFinder")}</button>
                    <button className="secondary" type="button" onClick={useLatestForProcessing} disabled={busy || !latestRecording.processingAvailable}>{text("selectForProcessing")}</button>
                  </div>
                  {!latestRecording.sourceMasterAvailable && <p className="warning">{text("noSourceMaster")}</p>}
                </>
              )}
              {latestRecording && (
                <div className="metrics">
                  <span><b>{latestRecording.durationSeconds?.toFixed(1) ?? "—"}</b> {text("seconds")}</span>
                  {lastResult?.validated && <>
                    <span><b>{lastResult.videoStreamCount}</b> {text("videoStreams")}</span>
                    <span><b>{lastResult.audioStreamCount}</b> {text("audioStreams")}</span>
                  </>}
                </div>
              )}
            </div>
          )}
          <ProcessingRunsView runs={[]} updates={processingRunUpdates.filter(u=>!history.some(i=>i.manifestPath===u.manifestPath && i.jobId===u.run.jobId))} language={language} busy={busy} reveal={path=>void revealPath(path)}/>
          {historyWarnings.length > 0 && <details className="warning">
            <summary>{text("historyNotices", { count: historyWarnings.length })}</summary>
            {historyWarnings.map((warning, index) => (
            <div className="warning" key={index}><MessageView message={warning} language={language} /></div>
            ))}
          </details>}
        </section>
      </div>
      <AppExitDialog state={exitState} language={language} choose={choice=>void chooseExit(choice)}/>
      <dialog ref={splitDialog} className="split-dialog" aria-labelledby="split-details-title" onCancel={() => setSplitDetails(null)} onClose={() => setSplitDetails(null)}>
        {splitDetails && <>
          <h2 id="split-details-title">{text("splitDetailsTitle")}</h2>
          <p>{text(splitDetails.linked ? "splitLinked" : "splitIndependent")}</p>
          <label>{text("splitThresholdMinutes")}<input autoFocus type="text" inputMode="numeric" value={splitDetails.threshold}
            aria-invalid={parseSplitMinutes(splitDetails.threshold) === null}
            onChange={event => setSplitDetails(changeSplitDraft(splitDetails, "threshold", event.target.value))} /></label>
          <label>{text("splitSegmentMinutes")}<input type="text" inputMode="numeric" value={splitDetails.segment}
            aria-invalid={parseSplitMinutes(splitDetails.segment) === null}
            onChange={event => setSplitDetails(changeSplitDraft(splitDetails, "segment", event.target.value))} /></label>
          <button className="secondary" type="button" onClick={() => setSplitDetails(linkSplitDraft(splitDetails))}>{text("splitRelink")}</button>
          <p className="split-help">{text(applySplitDraft(splitSettings, splitDetails) ? "splitInputHelp" : "processing.split_invalid", { max: MAX_SPLIT_MINUTES })}</p>
          <div className="split-dialog-actions">
            <button className="secondary" type="button" onClick={() => setSplitDetails(null)}>{text("splitCancel")}</button>
            <button className="secondary" type="button" disabled={!applySplitDraft(splitSettings, splitDetails)} onClick={() => {
              const applied = applySplitDraft(splitSettings, splitDetails);
              if (applied) { setSplitSettings(applied); setSegmentDraft(String(applied.splitSegmentMinutes)); setSplitDetails(null); }
            }}>{text("splitApply")}</button>
          </div>
        </>}
      </dialog>
      <dialog ref={outputDialog} className="split-dialog" onCancel={() => setOutputChoice(null)} aria-labelledby="output-choice-heading">
        {outputChoice && <>
          <h2 id="output-choice-heading">{text("outputAlternativeTitle")}</h2>
          <p>{text("outputAlternativeDescription", {width:outputChoice.width,height:outputChoice.height,altWidth:outputChoice.alternative.width,altHeight:outputChoice.alternative.height})}</p>
          <p>{text("outputAlternativeWarning")}</p>
          {error && <MessageView message={error} language={language} />}
          <div className="capture-scope-buttons">
            <button className="secondary" onClick={() => setOutputChoice(null)}>{text("splitCancel")}</button>
            <button onClick={() => {const choice=outputChoice;setOutputChoice(null);setError(null);void runCaptureRequest({...choice.request,approvedOutput:choice.alternative});}}>{text("outputAlternativeAccept")}</button>
          </div>
        </>}
      </dialog>
    </main>
  );
}
