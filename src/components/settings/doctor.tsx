import { useCallback, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useTranslation } from "react-i18next";
import { Bug, Check, Copy, Loader2, Stethoscope, Wrench } from "lucide-react";
import { formatDateTime } from "../../lib/format";
import { openGitHubBugReport } from "../../lib/githubIssue";
import { DoctorCheck, DoctorReport } from "../../types";
import { Card } from "./kit";

/* Doctor — one-shot diagnostics over the index, watcher, database and
   models. Never auto-runs on mount; failures degrade to a calm inline
   card because the backend command may not exist yet. */

type Phase = "idle" | "running" | "done" | "error";

const RANK: Record<DoctorCheck["status"], number> = { fail: 0, warn: 1, ok: 2 };
const WORD_KEY: Record<DoctorCheck["status"], "doctor.ok" | "doctor.warning" | "doctor.fail"> = {
  ok: "doctor.ok",
  warn: "doctor.warning",
  fail: "doctor.fail",
};
const DOT: Record<DoctorCheck["status"], string> = {
  ok: "bg-success",
  warn: "bg-warning",
  fail: "bg-danger",
};
const TONE: Record<DoctorCheck["status"], string> = {
  ok: "text-success",
  warn: "text-warning",
  fail: "text-danger",
};

/** Fails first, then warns, then ok; order otherwise stays as reported. */
function sortChecks(checks: DoctorCheck[]): DoctorCheck[] {
  return checks
    .map((c, i) => ({ c, i }))
    .sort((a, b) => RANK[a.c.status] - RANK[b.c.status] || a.i - b.i)
    .map((x) => x.c);
}

export function DoctorSection() {
  const { t, i18n } = useTranslation("settings");
  const [phase, setPhase] = useState<Phase>("idle");
  const [report, setReport] = useState<DoctorReport | null>(null);
  const [copied, setCopied] = useState(false);
  const [copiedFix, setCopiedFix] = useState<string | null>(null);
  /* One repair at a time, and the outcome shown inline: a button that does
     something invisible is worse than no button. */
  const [runningAction, setRunningAction] = useState<string | null>(null);
  const [actionResult, setActionResult] = useState<{ id: string; ok: boolean; text: string } | null>(
    null,
  );
  const [reported, setReported] = useState(false);

  const run = useCallback(() => {
    setPhase("running");
    setCopied(false);
    setCopiedFix(null);
    invoke<DoctorReport>("run_doctor")
      .then((res) => {
        setReport(res);
        setPhase("done");
      })
      .catch((err) => {
        console.warn("run_doctor not available:", err);
        setPhase("error");
      });
  }, []);

  const copyReport = useCallback(async () => {
    if (!report) return;
    try {
      await navigator.clipboard.writeText(JSON.stringify(report, null, 2));
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1500);
    } catch (err) {
      console.error("Copy failed:", err);
    }
  }, [report]);

  const copyFix = useCallback(async (id: string, fix: string) => {
    try {
      await navigator.clipboard.writeText(fix);
      setCopiedFix(id);
      window.setTimeout(() => setCopiedFix(null), 1500);
    } catch (err) {
      console.error("Copy failed:", err);
    }
  }, []);

  const runAction = useCallback(
    async (checkId: string, kind: string) => {
      setRunningAction(kind);
      setActionResult(null);
      try {
        const message = await invoke<string>("run_doctor_action", { action: kind });
        setActionResult({ id: checkId, ok: true, text: message });
        // Re-run the checks so the row reflects the repair rather than the
        // state that prompted it.
        invoke<DoctorReport>("run_doctor")
          .then(setReport)
          .catch(() => undefined);
      } catch (err) {
        setActionResult({ id: checkId, ok: false, text: String(err) });
      } finally {
        setRunningAction(null);
      }
    },
    [],
  );

  const handleReportGitHub = useCallback(async () => {
    const { copied } = await openGitHubBugReport({ report });
    if (copied) {
      setCopied(true);
      window.setTimeout(() => setCopied(false), 2000);
    }
    setReported(true);
    window.setTimeout(() => setReported(false), 2500);
  }, [report]);

  const checks = report ? sortChecks(report.checks) : [];
  const summary = report?.summary ?? { ok: 0, warn: 0, fail: 0 };
  const total = summary.ok + summary.warn + summary.fail;

  return (
    <div>
      {/* Toolbar: actions right-aligned; the section header above carries
          the description. */}
      <div className="mb-3 flex items-center justify-end gap-1.5">
        {report && phase !== "running" && (
          <>
            <button
              type="button"
              onClick={handleReportGitHub}
              className="btn btn-secondary h-8 px-3 text-[12px]"
              title={t("doctor.reportTooltip")}
            >
              <Bug className="h-3 w-3 text-danger" />
              {reported ? t("doctor.reportOpened") : t("doctor.reportIssue")}
            </button>
            <button
              type="button"
              onClick={copyReport}
              className="btn btn-secondary h-8 px-3 text-[12px]"
            >
              {copied ? <Check className="h-3 w-3 text-accent" /> : <Copy className="h-3 w-3" />}
              {t("doctor.copyReport")}
            </button>
          </>
        )}
        <button
          type="button"
          onClick={run}
          disabled={phase === "running"}
          className="btn btn-primary h-8 px-4 text-[12px]"
        >
          <Stethoscope className="h-3 w-3" />
          {t("doctor.runChecks")}
        </button>
      </div>

      {phase === "idle" && (
        <Card>
          <div className="flex flex-col items-center gap-2 px-4 py-10 text-center">
            <span className="flex h-9 w-9 items-center justify-center rounded-full border border-line bg-accent-soft text-subtle">
              <Stethoscope className="h-4 w-4" />
            </span>
            <p className="text-[13px] font-medium text-fg">{t("doctor.idleTitle")}</p>
            <p className="max-w-sm text-[12px] leading-[18px] text-muted">
              {t("doctor.idleDescription")}
            </p>
          </div>
        </Card>
      )}

      {phase === "running" && (
        <Card title={t("doctor.running")}>
          <div className="space-y-0">
            {[0, 1, 2, 3].map((i) => (
              <div key={i} className="flex items-center gap-3 border-b border-line px-4 py-2.5 last:border-b-0">
                <span className="h-2 w-2 shrink-0 animate-pulse rounded-full bg-line" />
                <span
                  className="h-2.5 animate-pulse rounded-full bg-line"
                  style={{ width: `${34 + ((i * 23) % 40)}%` }}
                />
                <span className="ml-auto h-2 w-8 shrink-0 animate-pulse rounded-full bg-line" />
              </div>
            ))}
          </div>
        </Card>
      )}

      {phase === "error" && (
        <div className="overflow-hidden rounded-panel border border-danger/30 bg-elevated">
          <p className="p-4 text-[12px] font-medium text-danger">{t("doctor.unavailableTitle")}</p>
          <p className="px-4 pb-3 text-[12px] leading-[18px] text-muted">
            {t("doctor.unavailableDescription")}
          </p>
        </div>
      )}

      {phase === "done" && report && (
        <Card>
          {/* Summary line with semantic-colored counts. */}
          <div className="flex items-center justify-between gap-4 border-b border-line bg-accent-soft px-4 py-2.5">
            <p className="text-[12px] text-muted">
              {t("doctor.totalChecks", { total })} ·{" "}
              <span className="text-success">{t("doctor.okCount", { count: summary.ok })}</span> ·{" "}
              <span className="text-warning">
                {t("doctor.warnCount", { count: summary.warn })}
              </span>
              {summary.fail > 0 && (
                <>
                  {" · "}
                  <span className="text-danger">{t("doctor.failCount", { count: summary.fail })}</span>
                </>
              )}
            </p>
            {report.ran_at > 0 && (
              <p className="shrink-0 font-mono text-[11px] text-subtle">
                {formatDateTime(report.ran_at, i18n.language)}
              </p>
            )}
          </div>

          {(summary.fail > 0 || summary.warn > 0) && (
            <div className="flex items-center justify-between gap-3 border-b border-line bg-warning/5 px-4 py-2.5">
              <p className="text-[12px] text-muted">
                {t("doctor.hasIssuesNotice")}
              </p>
              <button
                type="button"
                onClick={handleReportGitHub}
                className="btn btn-secondary h-7 shrink-0 px-2.5 text-[11px]"
              >
                <Bug className="h-3 w-3 text-warning" />
                {t("doctor.reportIssue")}
              </button>
            </div>
          )}

          {checks.map((c) => (
            <div key={c.id} className="flex gap-3 border-b border-line px-4 py-2.5 last:border-b-0">
              <span className={`mt-[5px] h-2 w-2 shrink-0 rounded-full ${DOT[c.status]}`} />
              <div className="min-w-0 flex-1">
                <p className="text-[13px] font-medium text-fg">{c.label}</p>
                {c.detail && (
                  <p className="break-words text-[12px] leading-[18px] text-muted">{c.detail}</p>
                )}
                {/* A repair the check knows how to run. Rendered before the
                    fix text, because doing it beats copying instructions. */}
                {c.action && (
                  <div className="mt-1.5 flex flex-wrap items-center gap-2">
                    <button
                      type="button"
                      disabled={runningAction !== null}
                      onClick={() => runAction(c.id, c.action!.kind)}
                      className="btn btn-secondary h-7 px-2.5 text-[11px]"
                    >
                      {runningAction === c.action.kind ? (
                        <>
                          <Loader2 className="h-3 w-3 animate-spin" />
                          {t("doctor.working")}
                        </>
                      ) : (
                        <>
                          <Wrench className="h-3 w-3" />
                          {c.action.label}
                        </>
                      )}
                    </button>
                  </div>
                )}

                {actionResult?.id === c.id && (
                  <p
                    className={`mt-1 break-words text-[11px] leading-[16px] ${
                      actionResult.ok ? "text-success" : "text-danger"
                    }`}
                  >
                    {actionResult.text}
                  </p>
                )}

                {c.fix && (
                  <div className="mt-1 flex min-w-0 items-start gap-1.5">
                    <code className="min-w-0 select-text break-all rounded bg-accent-soft px-1.5 py-0.5 font-mono text-[11px] text-muted">
                      {c.fix}
                    </code>
                    <button
                      type="button"
                      title={t("doctor.copyFix")}
                      onClick={() => copyFix(c.id, c.fix ?? "")}
                      className="mt-0.5 flex h-5 w-5 shrink-0 items-center justify-center rounded text-subtle transition-colors hover:bg-accent-soft hover:text-fg"
                    >
                      {copiedFix === c.id ? (
                        <Check className="h-3 w-3 text-accent" />
                      ) : (
                        <Copy className="h-3 w-3" />
                      )}
                    </button>
                  </div>
                )}
              </div>
              <span className={`shrink-0 text-[11px] font-medium ${TONE[c.status]}`}>
                {t(WORD_KEY[c.status])}
              </span>
            </div>
          ))}
        </Card>
      )}
    </div>
  );
}
