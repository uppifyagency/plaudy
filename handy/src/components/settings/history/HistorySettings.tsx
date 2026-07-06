import React, { useCallback, useEffect, useRef, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import { readFile } from "@tauri-apps/plugin-fs";
import { FolderOpen, Search } from "lucide-react";
import { useTranslation } from "react-i18next";
import {
  commands,
  events,
  type HistoryEntry,
  type HistoryUpdatePayload,
  type SessionOverview,
} from "@/bindings";
import { useOsType } from "@/hooks/useOsType";
import { DetailPane } from "./DetailPane";
import { IconButton } from "./IconButton";
import { ListRow } from "./ListRow";

const PAGE_SIZE = 30;

/** Day bucket label for the master list: Today / Yesterday / long date. */
function dayLabel(
  tsSeconds: number,
  lang: string,
  t: (k: string) => string,
): string {
  const d = new Date(tsSeconds * 1000);
  const today = new Date();
  // Calendar arithmetic (not "now minus 24h") so DST-shifted days still match.
  const yesterday = new Date(today);
  yesterday.setDate(yesterday.getDate() - 1);
  const sameDay = (a: Date, b: Date) => a.toDateString() === b.toDateString();
  if (sameDay(d, today)) return t("settings.history.today");
  if (sameDay(d, yesterday)) return t("settings.history.yesterday");
  return d.toLocaleDateString(lang, {
    day: "numeric",
    month: "long",
    year: "numeric",
  });
}

/** The Workstation: searchable date-grouped master list + rich detail pane. */
export const HistorySettings: React.FC = () => {
  const { t, i18n } = useTranslation();
  const osType = useOsType();
  const [entries, setEntries] = useState<HistoryEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [hasMore, setHasMore] = useState(true);
  const [selectedId, setSelectedId] = useState<number | null>(null);
  const [query, setQuery] = useState("");
  const [searchResults, setSearchResults] = useState<HistoryEntry[] | null>(
    null,
  );
  const sentinelRef = useRef<HTMLDivElement>(null);
  const entriesRef = useRef<HistoryEntry[]>([]);
  const loadingRef = useRef(false);
  // Batched per-session facts (speakers, duration), keyed by history id.
  // `null` = fetched, entry has no segments (cached so we never refetch it in
  // a loop); missing key = not fetched yet. Rows treat both as "no overview".
  const [overviews, setOverviews] = useState<
    Map<number, SessionOverview | null>
  >(new Map());
  // Ids already requested (mutated synchronously so concurrent pages/events
  // don't double-fetch the same id).
  const requestedOverviewIdsRef = useRef<Set<number>>(new Set());

  useEffect(() => {
    entriesRef.current = entries;
  }, [entries]);

  /** Fetch overviews for `ids` in ONE IPC call, merging into the map.
   *  Skips ids already requested unless `force` (used on "updated", where the
   *  entry may have just finished transcribing). */
  const fetchOverviews = useCallback(async (ids: number[], force = false) => {
    const requested = requestedOverviewIdsRef.current;
    const wanted = force
      ? Array.from(new Set(ids))
      : Array.from(new Set(ids.filter((id) => !requested.has(id))));
    if (wanted.length === 0) return;
    for (const id of wanted) requested.add(id);
    try {
      const result = await commands.getSessionOverviews(wanted);
      if (result.status !== "ok") throw new Error(String(result.error));
      setOverviews((prev) => {
        const next = new Map(prev);
        // Absent from the result = no segments; cache that as null.
        for (const id of wanted) next.set(id, null);
        for (const overview of result.data)
          next.set(overview.history_id, overview);
        return next;
      });
    } catch (error) {
      console.error("Failed to load session overviews:", error);
      // Un-mark so a later page load / update event can retry.
      for (const id of wanted) requested.delete(id);
    }
  }, []);

  const loadPage = useCallback(
    async (cursor?: number) => {
      const isFirstPage = cursor === undefined;
      if (!isFirstPage && loadingRef.current) return;
      loadingRef.current = true;
      if (isFirstPage) setLoading(true);
      try {
        const result = await commands.getHistoryEntries(
          cursor ?? null,
          PAGE_SIZE,
        );
        if (result.status === "ok") {
          const { entries: newEntries, has_more } = result.data;
          setEntries((prev) =>
            isFirstPage ? newEntries : [...prev, ...newEntries],
          );
          setHasMore(has_more);
          fetchOverviews(newEntries.map((e) => e.id));
        }
      } catch (error) {
        console.error("Failed to load history entries:", error);
      } finally {
        setLoading(false);
        loadingRef.current = false;
      }
    },
    [fetchOverviews],
  );

  useEffect(() => {
    loadPage();
  }, [loadPage]);

  // Debounced search; empty query returns to the paged list.
  useEffect(() => {
    const q = query.trim();
    if (!q) {
      setSearchResults(null);
      return;
    }
    const handle = setTimeout(async () => {
      try {
        const result = await commands.searchHistoryEntries(q, 100);
        if (result.status === "ok") {
          setSearchResults(result.data);
          fetchOverviews(result.data.map((e) => e.id));
        }
      } catch (error) {
        console.error("Search failed:", error);
      }
    }, 250);
    return () => clearTimeout(handle);
  }, [query, fetchOverviews]);

  // Infinite scroll via IntersectionObserver (paged list only).
  useEffect(() => {
    if (loading || searchResults !== null) return;
    const sentinel = sentinelRef.current;
    if (!sentinel || !hasMore) return;
    const observer = new IntersectionObserver(
      (observerEntries) => {
        if (observerEntries[0].isIntersecting) {
          const lastEntry = entriesRef.current[entriesRef.current.length - 1];
          if (lastEntry) loadPage(lastEntry.id);
        }
      },
      { threshold: 0 },
    );
    observer.observe(sentinel);
    return () => observer.disconnect();
  }, [loading, hasMore, loadPage, searchResults]);

  // Live updates from the transcription pipeline.
  useEffect(() => {
    const unlisten = events.historyUpdatePayload.listen((event) => {
      const payload: HistoryUpdatePayload = event.payload;
      if (payload.action === "added") {
        // Guard against duplicates: the entry may already be in the list
        // (e.g. an event replay or a page that raced the insert).
        setEntries((prev) =>
          prev.some((e) => e.id === payload.entry.id)
            ? prev
            : [payload.entry, ...prev],
        );
        fetchOverviews([payload.entry.id]);
      } else if (payload.action === "updated") {
        setEntries((prev) =>
          prev.map((e) => (e.id === payload.entry.id ? payload.entry : e)),
        );
        setSearchResults((prev) =>
          prev
            ? prev.map((e) => (e.id === payload.entry.id ? payload.entry : e))
            : prev,
        );
        // The entry may have just finished transcribing: refetch its overview.
        fetchOverviews([payload.entry.id], true);
      }
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, [fetchOverviews]);

  const displayed = searchResults ?? entries;

  // Keep a valid selection: default to the newest visible entry.
  useEffect(() => {
    if (displayed.length === 0) {
      setSelectedId(null);
      return;
    }
    if (!displayed.some((e) => e.id === selectedId)) {
      setSelectedId(displayed[0].id);
    }
  }, [displayed, selectedId]);

  const toggleSaved = async (id: number) => {
    const flip = (list: HistoryEntry[]) =>
      list.map((e) => (e.id === id ? { ...e, saved: !e.saved } : e));
    setEntries(flip);
    setSearchResults((prev) => (prev ? flip(prev) : prev));
    try {
      const result = await commands.toggleHistoryEntrySaved(id);
      if (result.status !== "ok") {
        setEntries(flip);
        setSearchResults((prev) => (prev ? flip(prev) : prev));
      }
    } catch (error) {
      console.error("Failed to toggle saved status:", error);
      setEntries(flip);
      setSearchResults((prev) => (prev ? flip(prev) : prev));
    }
  };

  const getAudioUrl = useCallback(
    async (fileName: string) => {
      try {
        const result = await commands.getAudioFilePath(fileName);
        if (result.status === "ok") {
          if (osType === "linux") {
            const fileData = await readFile(result.data);
            const blob = new Blob([fileData], { type: "audio/wav" });
            return URL.createObjectURL(blob);
          }
          return convertFileSrc(result.data, "asset");
        }
        return null;
      } catch (error) {
        console.error("Failed to get audio file path:", error);
        return null;
      }
    },
    [osType],
  );

  const deleteAudioEntry = async (id: number) => {
    // Optimistic removal; on failure restore the exact lists we had (a page-1
    // refetch would collapse a deep-scrolled list).
    const prevEntries = entries;
    const prevSearchResults = searchResults;
    setEntries((prev) => prev.filter((e) => e.id !== id));
    setSearchResults((prev) => (prev ? prev.filter((e) => e.id !== id) : prev));
    const restore = () => {
      setEntries(prevEntries);
      setSearchResults(prevSearchResults);
    };
    try {
      const result = await commands.deleteHistoryEntry(id);
      if (result.status !== "ok") {
        restore();
        throw new Error(String(result.error));
      }
    } catch (error) {
      restore();
      throw error;
    }
  };

  const retryHistoryEntry = async (id: number) => {
    const result = await commands.retryHistoryEntryTranscription(id);
    if (result.status !== "ok") {
      throw new Error(String(result.error));
    }
  };

  const openRecordingsFolder = async () => {
    try {
      const result = await commands.openRecordingsFolder();
      if (result.status !== "ok") throw new Error(String(result.error));
    } catch (error) {
      console.error("Failed to open recordings folder:", error);
    }
  };

  // Group the visible entries by day, preserving order (newest first).
  const groups: { label: string; items: HistoryEntry[] }[] = [];
  for (const entry of displayed) {
    const label = dayLabel(Number(entry.timestamp), i18n.language, t);
    const last = groups[groups.length - 1];
    if (last && last.label === label) last.items.push(entry);
    else groups.push({ label, items: [entry] });
  }

  const selectedEntry = displayed.find((e) => e.id === selectedId) ?? null;

  return (
    <div className="flex h-full min-h-0 w-full gap-4">
      {/* Master list */}
      <div className="flex w-72 shrink-0 flex-col gap-3">
        <div className="glass-chip flex items-center gap-2 px-3 py-1.5">
          <Search className="h-4 w-4 shrink-0 text-text/40" />
          <input
            type="text"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder={t("settings.history.searchPlaceholder")}
            className="min-w-0 flex-1 bg-transparent text-sm outline-none placeholder:text-text/35"
          />
          <IconButton
            onClick={openRecordingsFolder}
            title={t("settings.history.openFolder")}
          >
            <FolderOpen className="h-4 w-4" />
          </IconButton>
        </div>

        <div className="glass-panel min-h-0 flex-1 overflow-y-auto p-2">
          {loading ? (
            <p className="px-2 py-3 text-center text-sm text-text/60">
              {t("settings.history.loading")}
            </p>
          ) : displayed.length === 0 ? (
            <p className="px-2 py-3 text-center text-sm text-text/60">
              {searchResults !== null
                ? t("settings.history.noResults")
                : t("settings.history.empty")}
            </p>
          ) : (
            <>
              {groups.map((group) => (
                <div key={group.label} className="mb-1">
                  <p className="px-2.5 pb-1 pt-2 text-xs font-medium uppercase tracking-wide text-text/40">
                    {group.label}
                  </p>
                  <div className="flex flex-col gap-0.5">
                    {group.items.map((entry) => (
                      <ListRow
                        key={entry.id}
                        entry={entry}
                        overview={overviews.get(entry.id) ?? undefined}
                        selected={entry.id === selectedId}
                        onSelect={() => setSelectedId(entry.id)}
                      />
                    ))}
                  </div>
                </div>
              ))}
              {searchResults === null && (
                <div ref={sentinelRef} className="h-1" />
              )}
            </>
          )}
        </div>
      </div>

      {/* Detail pane */}
      {selectedEntry ? (
        <DetailPane
          entry={selectedEntry}
          onToggleSaved={() => toggleSaved(selectedEntry.id)}
          getAudioUrl={getAudioUrl}
          deleteAudio={deleteAudioEntry}
          retryTranscription={retryHistoryEntry}
        />
      ) : (
        <div className="glass-panel flex min-w-0 flex-1 items-center justify-center">
          <p className="text-sm text-text/40">
            {t("settings.history.selectEntry")}
          </p>
        </div>
      )}
    </div>
  );
};
