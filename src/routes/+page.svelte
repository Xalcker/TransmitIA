<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { revealItemInDir } from "@tauri-apps/plugin-opener";
  import { open as openDialog } from "@tauri-apps/plugin-dialog";
  import Stage from "$lib/Stage.svelte";
  import Posters from "$lib/Posters.svelte";
  import { onDestroy, onMount } from "svelte";
  import { translate, type Lang } from "$lib/i18n";
  import type { EncoderInfo, Fit, Layer, Mode, OutputStatus, Provider, Rect, Settings, SourceInfo, Stats, TextStyle, TranscriptEvent } from "$lib/types";

  let settings = $state<Settings | null>(null);
  let videoSources = $state<SourceInfo[]>([]);
  let audioSources = $state<SourceInfo[]>([]);
  let encoders = $state<EncoderInfo[]>([]);
  let mode = $state<Mode>("stopped");
  let stats = $state<Stats | null>(null);
  let peaks = $state<[number, number]>([0, 0]);
  let inputPeaks = $state<Record<string, [number, number]>>({});
  let outStatus = $state<Record<string, OutputStatus>>({});
  // Transcripción: segmentos en orden de llegada.
  let segments = $state<{ id: string; text: string; final: boolean; translation?: string }[]>([]);
  let tlStatus = $state<{ state: string; message: string | null } | null>(null);
  let tlKeyInput = $state("");
  let models = $state<Record<string, string[]>>({});
  let modelsError = $state<string | null>(null);
  let trStatus = $state<{ state: string; message: string | null } | null>(null);
  let apiKeys = $state<Record<string, boolean>>({});
  let apiKeyInput = $state("");
  let transcriptBox = $state<HTMLDivElement | undefined>();
  let frame = $state<string | null>(null);
  let error = $state<string | null>(null);
  let busy = $state(false);
  /** Claves escritas pero aún no guardadas, por destino. */
  let keyInputs = $state<Record<string, string>>({});
  let keyStatus = $state<Record<string, boolean>>({});
  let selectedId = $state<string | null>(null);
  let tab = $state<"scene" | "output" | "ai">("scene");
  let addChoice = $state("");

  const lang = $derived<Lang>(settings?.language ?? "es");
  const t = (k: string) => translate(lang, k);
  const live = $derived(mode === "live");
  const portrait = $derived(settings?.orientation === "portrait");
  const scene = $derived(settings ? settings.scenes[settings.activeScene] : null);
  const selected = $derived(scene?.layers.find((l) => l.id === selectedId) ?? null);
  const canvas = $derived.by(() => {
    const [a, b] = settings?.resolution === "720p" ? [1280, 720] : [1920, 1080];
    return portrait ? { w: b, h: a } : { w: a, h: b };
  });
  const autoBitrate = $derived.by(() => {
    if (!settings) return 0;
    const hd = settings.resolution === "1080p";
    return settings.fps === 60 ? (hd ? 6000 : 4500) : hd ? 4500 : 3000;
  });

  const unlisten: UnlistenFn[] = [];

  onMount(async () => {
    unlisten.push(
      await listen<string>("preview-frame", (e) => (frame = "data:image/jpeg;base64," + e.payload)),
      await listen<{ master: [number, number]; inputs: Record<string, [number, number]> }>("audio-levels", (e) => {
        peaks = e.payload.master;
        inputPeaks = e.payload.inputs;
      }),
      await listen<OutputStatus>("output-status", (e) => (outStatus[e.payload.id] = e.payload)),
      await listen<TranscriptEvent>("transcript", (e) => onTranscript(e.payload)),
      await listen<{ state: string; message: string | null }>("transcription-status", (e) => (trStatus = e.payload)),
      await listen<{ itemId: string; text: string }>("translation", (e) => {
        const seg = segments.find((x) => x.id === e.payload.itemId);
        if (seg) seg.translation = e.payload.text;
        tlStatus = { state: "active", message: null };
      }),
      await listen<{ state: string; message: string | null }>("translation-status", (e) => (tlStatus = e.payload)),
      await listen<Stats>("stats", (e) => (stats = e.payload)),
      await listen<string>("engine-error", (e) => (error = e.payload)),
    );
    settings = await invoke<Settings>("load_settings");
    encoders = await invoke<EncoderInfo[]>("list_encoders");
    await refreshKeyStatus();
    apiKeys = await invoke<Record<string, boolean>>("api_key_status");
    await refreshSources(false);
    await startPreview();
  });

  onDestroy(() => unlisten.forEach((u) => u()));

  async function refreshSources(refresh: boolean) {
    const s = await invoke<{ video: SourceInfo[]; audio: SourceInfo[] }>("list_sources", { refresh });
    videoSources = s.video;
    audioSources = s.audio;
  }

  function layerLabel(l: Layer) {
    if (l.source.startsWith("image:")) return l.source.split(/[\\/]/).pop() ?? t("image_file");
    if (l.source.startsWith("color:")) return `${t("solid_color")} ${l.source.slice(6)}`;
    if (l.text) return `${t("text")}: ${l.text.content.split("\n")[0] || "…"}`;
    const s = videoSources.find((v) => v.id === l.source);
    return s ? sourceLabel(s) : l.source;
  }

  // ---------- Capas ----------
  function newId() {
    return crypto.randomUUID?.() ?? `layer-${Date.now()}-${Math.random().toString(36).slice(2)}`;
  }

  async function addLayer(choice: string) {
    if (!settings || !scene || !choice) return;
    let source = choice;
    if (choice === "__image") {
      const path = await openDialog({ multiple: false, filters: [{ name: "Image", extensions: ["png", "jpg", "jpeg"] }] });
      if (typeof path !== "string") return;
      source = `image:${path}`;
    } else if (choice === "__color") {
      source = "color:#1e293b";
    }
    const id = newId();
    if (choice === "__text") {
      const text: TextStyle = {
        content: t("text_default"),
        font: "Sans",
        bold: true,
        size: 45,
        color: "#ffffff",
        outline: true,
        background: "#000000",
        backgroundOpacity: 0.5,
        align: "center",
      };
      scene.layers.push({ id, source: `text:${id}`, rect: { x: 0.05, y: 0.8, w: 0.9, h: 0.12 }, fit: "contain", visible: true, text });
      selectedId = id;
      return;
    }
    // La primera capa ocupa todo el lienzo; las siguientes entran como recuadro.
    const first = scene.layers.length === 0;
    const rect: Rect = first ? { x: 0, y: 0, w: 1, h: 1 } : { x: 0.58, y: 0.58, w: 0.38, h: 0.38 };
    const layer: Layer = { id, source, rect, fit: first || choice === "__color" ? "cover" : "contain", visible: true };
    scene.layers.push(layer);
    selectedId = layer.id;
  }

  /** Pone un póster en la escena actual (en vivo se agrega sin reiniciar). */
  async function addPosterLayer(path: string) {
    if (!settings || !scene) return;
    const id = newId();
    scene.layers.push({ id, source: `image:${path}`, rect: { x: 0, y: 0, w: 1, h: 1 }, fit: "contain", visible: true });
    selectedId = id;
    if (live) {
      try {
        await invoke("add_layer_live", { settings: $state.snapshot(settings), layerId: id });
      } catch (e) {
        error = String(e);
      }
    }
  }

  function removeLayer(id: string) {
    if (!scene) return;
    scene.layers = scene.layers.filter((l) => l.id !== id);
    if (selectedId === id) selectedId = null;
  }

  /** dir = +1 sube (hacia el frente), -1 baja. */
  function moveLayer(id: string, dir: number) {
    if (!scene) return;
    const i = scene.layers.findIndex((l) => l.id === id);
    const j = i + dir;
    if (i < 0 || j < 0 || j >= scene.layers.length) return;
    const arr = scene.layers;
    [arr[i], arr[j]] = [arr[j], arr[i]];
  }

  function setRect(id: string, rect: Rect) {
    const l = scene?.layers.find((x) => x.id === id);
    if (l) l.rect = rect;
  }

  function centerLayer(l: Layer) {
    l.rect = { ...l.rect, x: (1 - l.rect.w) / 2, y: (1 - l.rect.h) / 2 };
  }

  function sourceLabel(s: SourceInfo) {
    if (s.kind === "screen") return `${t("screen")} ${s.label}`;
    if (s.kind === "test" || s.kind === "none") return t(s.label);
    return s.label;
  }

  async function run(fn: () => Promise<unknown>) {
    busy = true;
    error = null;
    try {
      await fn();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  async function startPreview() {
    if (!settings) return;
    await run(async () => {
      await invoke("start_preview", { settings: $state.snapshot(settings) });
      mode = "preview";
    });
  }

  async function goLive() {
    if (!settings) return;
    const active = settings.destinations.filter((d) => d.enabled && d.url.trim() !== "");
    if (!active.length && !settings.recording.enabled) {
      error = t("no_output");
      return;
    }
    // Claves escritas sin presionar "Guardar": se guardan ahora.
    for (const [id, k] of Object.entries(keyInputs)) if (k.trim()) await saveKey(id);
    const missing = active.find((d) => d.kind === "rtmp" && !keyStatus[d.id]);
    if (missing) {
      error = `${t("key_required")} (${missing.name || missing.url})`;
      return;
    }
    outStatus = {};
    await run(async () => {
      await invoke("go_live", { settings: $state.snapshot(settings) });
      mode = "live";
    });
  }

  async function endLive() {
    if (!settings) return;
    await run(async () => {
      await invoke("end_live", { settings: $state.snapshot(settings) });
      mode = "preview";
    });
  }

  async function refreshKeyStatus() {
    if (!settings) return;
    keyStatus = await invoke<Record<string, boolean>>("stream_key_status", { ids: settings.destinations.map((d) => d.id) });
  }

  async function saveKey(destId: string) {
    await run(async () => {
      await invoke("set_stream_key", { destId, key: keyInputs[destId] ?? "" });
      keyInputs[destId] = "";
      await refreshKeyStatus();
    });
  }

  function addDestination() {
    if (!settings) return;
    const n = settings.destinations.length + 1;
    settings.destinations.push({ id: newId(), name: `${t("destination")} ${n}`, enabled: true, kind: "rtmp", url: "" });
  }

  async function removeDestination(id: string) {
    if (!settings) return;
    await invoke("set_stream_key", { destId: id, key: "" }).catch(() => {});
    settings.destinations = settings.destinations.filter((d) => d.id !== id);
  }

  // ---------- Transcripción ----------
  function onTranscript(ev: TranscriptEvent) {
    let seg = segments.find((x) => x.id === ev.itemId);
    if (!seg) {
      seg = { id: ev.itemId, text: "", final: false };
      segments.push(seg);
      if (segments.length > 300) segments.splice(0, segments.length - 300);
      seg = segments[segments.length - 1];
    }
    if (ev.kind === "delta") seg.text += ev.text;
    else seg.text = ev.text;
    if (ev.kind === "final") seg.final = true;
    requestAnimationFrame(() => transcriptBox?.scrollTo({ top: transcriptBox.scrollHeight }));
  }

  const transcriptText = $derived(
    segments
      .filter((x) => x.text.trim())
      .map((x) => (x.translation ? `${x.text.trim()}\n${x.translation.trim()}` : x.text.trim()))
      .join("\n"),
  );

  async function saveTranslateKey(provider: Provider) {
    await run(async () => {
      await invoke("set_api_key", { provider, key: tlKeyInput });
      tlKeyInput = "";
      apiKeys = await invoke<Record<string, boolean>>("api_key_status");
    });
  }

  /** Consulta al proveedor qué modelos están disponibles con la clave guardada. */
  async function loadModels(provider: Provider, purpose: "text" | "transcribe") {
    modelsError = null;
    try {
      models[`${provider}:${purpose}`] = await invoke<string[]>("list_models", { provider, purpose });
    } catch (e) {
      modelsError = String(e) === "no-api-key" ? t("tr_no_key") : String(e);
    }
  }

  const TL_DEFAULT: Record<Provider, string> = { openai: "gpt-5-nano", gemini: "gemini-flash-latest" };

  async function saveApiKey(provider: Provider) {
    await run(async () => {
      await invoke("set_api_key", { provider, key: apiKeyInput });
      apiKeyInput = "";
      apiKeys = await invoke<Record<string, boolean>>("api_key_status");
    });
  }

  const LANGS = ["", "es", "en", "pt", "fr", "de", "it", "ja", "ko", "zh"];
  const DEFAULT_MODELS: Record<Provider, string> = { openai: "gpt-live-transcribe", gemini: "gemini-3.5-transcribe-live" };

  // ---------- Mezclador ----------
  function addAudio(source: string) {
    if (!settings || !source) return;
    settings.audioInputs.push({ id: newId(), source, volume: 1, muted: false });
  }

  function audioLabel(source: string) {
    const s = audioSources.find((a) => a.id === source);
    return s ? sourceLabel(s) : source;
  }

  const unusedAudio = $derived(audioSources.filter((a) => !settings?.audioInputs.some((i) => i.source === a.id)));
  let audioChoice = $state("");

  // Guarda la configuración. Si cambia la estructura (formato, fuentes, capas
  // añadidas/quitadas) se reinicia la vista previa; si solo cambia la
  // composición (posición, tamaño, escena, orden…) se aplica en vivo.
  let lastStructKey = "";
  let restartTimer: ReturnType<typeof setTimeout> | undefined;
  let layoutQueued = false;
  $effect(() => {
    if (!settings) return;
    const snap = $state.snapshot(settings);
    invoke("save_settings", { settings: snap }).catch(() => {});
    const struct = JSON.stringify([
      snap.resolution,
      snap.fps,
      snap.orientation,
      snap.audioInputs.map((a) => [a.id, a.source]),
      snap.transcription,
      snap.translation,
      snap.scenes.map((sc) => sc.layers.map((l) => [l.id, l.source]).sort()),
    ]);
    const structural = lastStructKey !== "" && struct !== lastStructKey;
    lastStructKey = struct;
    if (structural && mode === "preview") {
      clearTimeout(restartTimer);
      restartTimer = setTimeout(startPreview, 250);
    } else if (mode !== "stopped" && !layoutQueued) {
      layoutQueued = true;
      requestAnimationFrame(() => {
        layoutQueued = false;
        invoke("apply_layout", { settings: $state.snapshot(settings) }).catch(() => {});
      });
    }
  });

  // Traduce errores conocidos del motor a un mensaje claro.
  function friendly(msg: string) {
    if (msg.includes("permission:camera")) return t("perm_camera");
    if (msg.includes("source-stalled")) return t("source_stalled");
    if (msg.includes("permission:microphone")) return t("perm_mic");
    if (msg.includes("NetStream.Publish.Failed") || msg.includes("Publish Rejected")) return `${t("publish_rejected")} (${msg})`;
    return msg;
  }

  async function openLog() {
    const p = await invoke<string | null>("log_file_path");
    if (p) await revealItemInDir(p);
  }

  function fmtTime(sec: number) {
    const h = Math.floor(sec / 3600);
    const m = Math.floor((sec % 3600) / 60);
    const s = sec % 60;
    return [h, m, s].map((n) => String(n).padStart(2, "0")).join(":");
  }

  function meterPct(v: number) {
    if (v <= 0) return 0;
    const db = 20 * Math.log10(v);
    return Math.max(0, Math.min(100, ((db + 60) / 60) * 100));
  }
</script>

{#if settings}
  <div class="app">
    <header>
      <div class="brand">Transmit<span>IA</span></div>
      <div class="status {mode}">
        {#if live}
          <span class="dot"></span>
          {stats?.streaming ? t("status_live") : t("status_recording")}
          {#if stats}<span class="time">{fmtTime(stats.uptimeSec)}</span>{/if}
        {:else}
          {t(mode === "preview" ? "status_preview" : "status_stopped")}
        {/if}
      </div>
      <div class="lang">
        <button class:on={lang === "es"} onclick={() => (settings!.language = "es")}>ES</button>
        <button class:on={lang === "en"} onclick={() => (settings!.language = "en")}>EN</button>
      </div>
    </header>

    <nav class="scenes">
      {#each settings.scenes as sc, i}
        <button
          class:on={settings.activeScene === i}
          onclick={() => {
            settings!.activeScene = i;
            selectedId = null;
          }}
        >
          {t("scene")} {i + 1}
          <small>{sc.layers.length}</small>
        </button>
      {/each}
      {#if live}<span class="onair">● {t("on_air")}: {t("scene")} {settings.activeScene + 1}</span>{/if}
    </nav>

    <main>
      <section class="stage">
        <Stage
          frame={mode !== "stopped" ? frame : null}
          {canvas}
          layers={scene?.layers ?? []}
          {selectedId}
          labelOf={layerLabel}
          onselect={(id) => (selectedId = id)}
          onrect={setRect}
          emptyText={t("no_preview")}
        />
        {#if mode === "stopped"}
          <button class="ghost small" onclick={startPreview} disabled={busy}>{t("start_preview")}</button>
        {/if}

        <div class="meters">
          {#each peaks as p, i}
            <div class="meter" title={i === 0 ? "L" : "R"}>
              <div class="fill" style="width:{meterPct(p)}%"></div>
            </div>
          {/each}
        </div>

        <div class="stats">
          <span>{settings.resolution} · {settings.fps} fps · {portrait ? "9:16" : "16:9"}</span>
          {#if stats}
            <span>{stats.fps.toFixed(1)} fps</span>
            {#if live}<span>{Math.round(stats.videoKbps)} kbps</span>{/if}
            {#if stats.encoder}<span>{stats.encoder}</span>{/if}
          {/if}
          {#if live && stats?.recordingPath}<span class="path" title={stats.recordingPath}>{t("rec_file")}: {stats.recordingPath}</span>{/if}
        </div>

        {#if settings.transcription.enabled}
          <div class="transcript">
            <div class="thead">
              <span>{t("transcript")}</span>
              {#if trStatus}<span class="chip {trStatus.state === 'active' ? 'live' : trStatus.state === 'error' ? 'error' : 'connecting'}" title={trStatus.message ?? ""}>{t("tr_" + trStatus.state)}</span>{/if}
              {#if settings.translation.enabled}
                <span class="chip {tlStatus?.state === 'error' ? 'error' : tlStatus ? 'live' : ''}" title={tlStatus?.message ?? ""}>
                  → {settings.translation.target.toUpperCase()}{tlStatus?.state === "error" ? ` · ${t("tr_error")}` : ""}
                </span>
              {/if}
              <span class="inline right">
                <button class="ghost small" disabled={!transcriptText} onclick={() => navigator.clipboard.writeText(transcriptText)}>{t("copy")}</button>
                <button class="ghost small" disabled={!segments.length} onclick={() => (segments = [])}>{t("clear")}</button>
              </span>
            </div>
            <div class="tbody" bind:this={transcriptBox}>
              {#each segments as seg (seg.id)}
                <p class:pending={!seg.final}>{seg.text}</p>
                {#if seg.translation}<p class="tl">{seg.translation}</p>{/if}
              {:else}
                <p class="hint">{trStatus?.state === "error" && trStatus.message ? (trStatus.message === "no-api-key" ? t("tr_no_key") : trStatus.message === "no-audio" ? t("tr_no_audio") : trStatus.message === "captions-unavailable" ? t("cc_unavailable") : trStatus.message) : t("transcript_wait")}</p>
              {/each}
            </div>
          </div>
        {/if}

        {#if error}
          <div class="error" role="alert">
            <span>{friendly(error)}</span>
            <button class="ghost small" onclick={openLog}>{t("open_log")}</button>
            <button class="ghost" onclick={() => (error = null)}>×</button>
          </div>
        {/if}
      </section>

      <aside>
        <nav class="tabs">
          {#each [["scene", "tab_scene"], ["output", "tab_output"], ["ai", "tab_ai"]] as [id, label]}
            <button class:on={tab === id} onclick={() => (tab = id as typeof tab)}>{t(label)}</button>
          {/each}
        </nav>
        {#if tab === "scene"}
        <fieldset>
          <h2>{t("layers")} · {t("scene")} {settings.activeScene + 1}</h2>
          {#if scene && scene.layers.length}
            <ul class="layers">
              {#each [...scene.layers].reverse() as l (l.id)}
                <li class:sel={l.id === selectedId}>
                  <button class="icon" title={t("show_hide")} onclick={() => (l.visible = !l.visible)}>{l.visible ? "◉" : "○"}</button>
                  <button class="name" onclick={() => (selectedId = l.id)}>{layerLabel(l)}</button>
                  <button class="icon" title={t("move_up")} onclick={() => moveLayer(l.id, 1)}>▲</button>
                  <button class="icon" title={t("move_down")} onclick={() => moveLayer(l.id, -1)}>▼</button>
                  <button class="icon" title={t("delete")} disabled={live} onclick={() => removeLayer(l.id)}>✕</button>
                </li>
              {/each}
            </ul>
          {:else}
            <p class="hint">{t("no_layers")}</p>
          {/if}
          <div class="inline">
            <select bind:value={addChoice} disabled={live}>
              <option value="">+ {t("add_layer")}…</option>
              {#each videoSources as v}<option value={v.id}>{sourceLabel(v)}</option>{/each}
              <option value="__image">{t("image_file")}…</option>
              <option value="__color">{t("solid_color")}</option>
              <option value="__text">{t("text")}</option>
            </select>
            <button
              class="ghost small"
              disabled={live || !addChoice}
              onclick={async () => {
                await addLayer(addChoice);
                addChoice = "";
              }}>{t("add")}</button
            >
          </div>
          {#if live}<p class="hint">{t("live_locked")}</p>{/if}
          <button class="ghost small" disabled={live} onclick={() => refreshSources(true)}>↻ {t("refresh")}</button>

          {#if selected}
            <div class="props">
              <div class="row">
                <span>{t("fit")}</span>
                <div class="seg">
                  {#each ["contain", "cover", "stretch"] as const as f}
                    <button class:on={selected.fit === f} onclick={() => (selected!.fit = f as Fit)}>{t("fit_" + f)}</button>
                  {/each}
                </div>
              </div>
              {#if selected.text}
                {@const tx = selected.text}
                <textarea rows="2" bind:value={tx.content} placeholder={t("text")}></textarea>
                <div class="inline">
                  <select bind:value={tx.font}>
                    {#each ["Sans", "Serif", "Monospace", "Helvetica", "Arial", "Georgia", "Impact"] as f}<option value={f}>{f}</option>{/each}
                  </select>
                  <button class="ghost small" class:on={tx.bold} onclick={() => (tx.bold = !tx.bold)}><b>B</b></button>
                  <button class="ghost small" class:on={tx.outline} onclick={() => (tx.outline = !tx.outline)} title={t("outline")}>A̲</button>
                </div>
                <label class="row"><span>{t("text_size")}</span><input type="range" min="10" max="95" bind:value={tx.size} /></label>
                <div class="row">
                  <span>{t("align")}</span>
                  <div class="seg">
                    {#each [["left", "⟸"], ["center", "≡"], ["right", "⟹"]] as [a, icon]}
                      <button class:on={tx.align === a} onclick={() => (tx.align = a as TextStyle["align"])}>{icon}</button>
                    {/each}
                  </div>
                </div>
                <label class="row"><span>{t("text_color")}</span><input type="color" bind:value={tx.color} /></label>
                <label class="row">
                  <span>{t("background")}</span>
                  <span class="inline">
                    <input type="color" bind:value={tx.background} />
                    <input type="range" min="0" max="1" step="0.05" bind:value={tx.backgroundOpacity} />
                  </span>
                </label>
              {/if}
              {#if selected.source.startsWith("color:")}
                <label class="row">
                  <span>{t("solid_color")}</span>
                  <input
                    type="color"
                    disabled={live}
                    value={selected.source.slice(6)}
                    onchange={(e) => (selected!.source = "color:" + e.currentTarget.value)}
                  />
                </label>
              {/if}
              <div class="inline">
                <button class="ghost small" onclick={() => (selected!.rect = { x: 0, y: 0, w: 1, h: 1 })}>{t("full_canvas")}</button>
                <button class="ghost small" onclick={() => centerLayer(selected!)}>{t("center")}</button>
              </div>
              <p class="hint">
                {Math.round(selected.rect.x * canvas.w)}, {Math.round(selected.rect.y * canvas.h)} ·
                {Math.round(selected.rect.w * canvas.w)}×{Math.round(selected.rect.h * canvas.h)} px · {t("shift_hint")}
              </p>
            </div>
          {/if}
        </fieldset>

        <fieldset>
          <h2>{t("mixer")}</h2>
          {#each settings.audioInputs as a (a.id)}
            {@const p = inputPeaks[a.id] ?? [0, 0]}
            <div class="ainput">
              <div class="row">
                <span class="aname" title={audioLabel(a.source)}>{audioLabel(a.source)}</span>
                <span class="inline">
                  <button class="ghost small" class:muted={a.muted} onclick={() => (a.muted = !a.muted)}>{a.muted ? "🔇" : "🔊"}</button>
                  <button class="ghost small" disabled={live} title={t("delete")}
                    onclick={() => (settings!.audioInputs = settings!.audioInputs.filter((x) => x.id !== a.id))}>✕</button>
                </span>
              </div>
              <div class="meter small"><div class="fill" style="width:{meterPct(Math.max(p[0], p[1]))}%"></div></div>
              <input type="range" min="0" max="2" step="0.01" bind:value={a.volume} title="{Math.round(a.volume * 100)}%" />
            </div>
          {:else}
            <p class="hint">{t("no_audio")}</p>
          {/each}
          <div class="inline">
            <select bind:value={audioChoice} disabled={live}>
              <option value="">+ {t("add_audio")}…</option>
              {#each unusedAudio as s}<option value={s.id}>{sourceLabel(s)}</option>{/each}
            </select>
            <button class="ghost small" disabled={live || !audioChoice} onclick={() => { addAudio(audioChoice); audioChoice = ""; }}>{t("add")}</button>
          </div>
        </fieldset>

        {:else if tab === "output"}
        <fieldset disabled={live}>
          <h2>{t("format")}</h2>
          <div class="row">
            <span>{t("resolution")}</span>
            <div class="seg">
              <button class:on={settings.resolution === "1080p"} onclick={() => (settings!.resolution = "1080p")}>1080p</button>
              <button class:on={settings.resolution === "720p"} onclick={() => (settings!.resolution = "720p")}>720p</button>
            </div>
          </div>
          <div class="row">
            <span>{t("framerate")}</span>
            <div class="seg">
              <button class:on={settings.fps === 30} onclick={() => (settings!.fps = 30)}>30</button>
              <button class:on={settings.fps === 60} onclick={() => (settings!.fps = 60)}>60</button>
            </div>
          </div>
          <div class="row">
            <span>{t("aspect")}</span>
            <div class="seg">
              <button class:on={!portrait} onclick={() => (settings!.orientation = "landscape")}>16:9</button>
              <button class:on={portrait} onclick={() => (settings!.orientation = "portrait")}>9:16</button>
            </div>
          </div>
        </fieldset>

        <fieldset disabled={live}>
          <h2>{t("encoding")}</h2>
          <label>
            {t("encoder")}
            <select bind:value={settings.encoder}>
              <option value={null}>{t("auto")}</option>
              {#each encoders as e}
                <option value={e.id}>{e.label} · {e.hardware ? t("hardware") : t("software")}</option>
              {/each}
            </select>
          </label>
          <label>
            {t("bitrate")}
            <input
              type="number"
              min="500"
              max="20000"
              step="100"
              placeholder={`${t("auto")} (${autoBitrate})`}
              value={settings.videoBitrateKbps ?? ""}
              oninput={(e) => {
                const v = parseInt(e.currentTarget.value, 10);
                settings!.videoBitrateKbps = Number.isFinite(v) ? v : null;
              }}
            />
          </label>
        </fieldset>

        <fieldset>
          <h2>{t("destinations")}</h2>
          {#each settings.destinations as d (d.id)}
            {@const st = outStatus[d.id]}
            <div class="dest" class:off={!d.enabled}>
              <div class="row">
                <input class="dname" type="text" bind:value={d.name} disabled={live} />
                <span class="inline">
                  {#if live && d.enabled}
                    <span class="chip {st?.state ?? 'connecting'}" title={st?.message ?? ""}>
                      {t("out_" + (st?.state ?? "connecting"))}{st?.state === "live" ? ` · ${Math.round(st.kbps)} kbps` : ""}
                    </span>
                  {/if}
                  <input type="checkbox" class="switch" bind:checked={d.enabled} disabled={live} aria-label={t("enabled")} />
                  <button class="ghost small" disabled={live} title={t("delete")} onclick={() => removeDestination(d.id)}>✕</button>
                </span>
              </div>
              {#if live && st?.state === "error" && st.message}<p class="hint err">{friendly(st.message)}</p>{/if}
              {#if !live}
                <div class="row">
                  <span>{t("protocol")}</span>
                  <div class="seg">
                    <button class:on={d.kind === "rtmp"} onclick={() => (d.kind = "rtmp")}>RTMP</button>
                    <button class:on={d.kind === "srt"} onclick={() => (d.kind = "srt")}>SRT</button>
                  </div>
                </div>
                <input type="text" spellcheck="false" placeholder={t(d.kind === "rtmp" ? "url_hint_rtmp" : "url_hint_srt")} bind:value={d.url} />
                <label>
                  {t("stream_key")} <small class:ok={keyStatus[d.id]}>({keyStatus[d.id] ? t("key_saved") : t("key_missing")})</small>
                  <div class="inline">
                    <input type="password" autocomplete="off" bind:value={keyInputs[d.id]} placeholder="••••••••" />
                    <button class="ghost small" onclick={() => saveKey(d.id)} disabled={busy}>{t("save")}</button>
                  </div>
                </label>
              {/if}
            </div>
          {/each}
          <button class="ghost small" disabled={live} onclick={addDestination}>+ {t("add_destination")}</button>
        </fieldset>

        <fieldset disabled={live}>
          <h2>
            {t("recording")}
            <input type="checkbox" class="switch" bind:checked={settings.recording.enabled} aria-label={t("enabled")} />
          </h2>
          <label>
            {t("folder")}
            <input type="text" spellcheck="false" bind:value={settings.recording.folder} />
          </label>
        </fieldset>

        {:else}
        <fieldset disabled={live}>
          <h2>
            {t("transcription")}
            <input type="checkbox" class="switch" bind:checked={settings.transcription.enabled} aria-label={t("enabled")} />
          </h2>
          <div class="row">
            <span>{t("provider")}</span>
            <div class="seg">
              <button class:on={settings.transcription.provider === "openai"} onclick={() => (settings!.transcription.provider = "openai")}>OpenAI</button>
              <button class:on={settings.transcription.provider === "gemini"} onclick={() => (settings!.transcription.provider = "gemini")}>Gemini</button>
            </div>
          </div>
          <label>
            {t("api_key")} <small class:ok={apiKeys[settings.transcription.provider]}>({apiKeys[settings.transcription.provider] ? t("key_saved") : t("key_missing")})</small>
            <div class="inline">
              <input type="password" autocomplete="off" bind:value={apiKeyInput} placeholder="••••••••" />
              <button class="ghost small" onclick={() => saveApiKey(settings!.transcription.provider)} disabled={busy}>{t("save")}</button>
            </div>
          </label>
          <label>
            {t("model")}
            <div class="inline">
              <input type="text" spellcheck="false" list="tr-models" bind:value={settings.transcription.model} placeholder={DEFAULT_MODELS[settings.transcription.provider]} />
              <button class="ghost small" title={t("load_models")} onclick={() => loadModels(settings!.transcription.provider, "transcribe")}>↻</button>
            </div>
            <datalist id="tr-models">
              {#each models[`${settings.transcription.provider}:transcribe`] ?? [] as m}<option value={m}></option>{/each}
            </datalist>
          </label>
          <label>
            {t("tr_language")}
            <select bind:value={settings.transcription.language}>
              {#each LANGS as l}<option value={l}>{l ? l.toUpperCase() : t("auto")}</option>{/each}
            </select>
          </label>
          <label class="row">
            <span>{t("captions_608")}</span>
            <input type="checkbox" class="switch" bind:checked={settings.transcription.captions} />
          </label>
          <p class="hint">{t("tr_privacy")}</p>
        </fieldset>

        <fieldset disabled={live}>
          <h2>
            {t("translation")}
            <input type="checkbox" class="switch" bind:checked={settings.translation.enabled} aria-label={t("enabled")} />
          </h2>
          {#if !settings.transcription.enabled}<p class="hint">{t("tl_needs_tr")}</p>{/if}
          <label>
            {t("tl_target")}
            <select bind:value={settings.translation.target}>
              {#each LANGS.filter(Boolean) as l}<option value={l}>{l.toUpperCase()}</option>{/each}
            </select>
          </label>
          <div class="row">
            <span>{t("provider")}</span>
            <div class="seg">
              <button class:on={settings.translation.provider === "openai"} onclick={() => (settings!.translation.provider = "openai")}>OpenAI</button>
              <button class:on={settings.translation.provider === "gemini"} onclick={() => (settings!.translation.provider = "gemini")}>Gemini</button>
            </div>
          </div>
          <label>
            {t("api_key")} <small class:ok={apiKeys[settings.translation.provider]}>({apiKeys[settings.translation.provider] ? t("key_saved") : t("key_missing")})</small>
            <div class="inline">
              <input type="password" autocomplete="off" bind:value={tlKeyInput} placeholder="••••••••" />
              <button class="ghost small" onclick={() => saveTranslateKey(settings!.translation.provider)} disabled={busy}>{t("save")}</button>
            </div>
          </label>
          <label>
            {t("model")}
            <div class="inline">
              <input type="text" spellcheck="false" list="tl-models" bind:value={settings.translation.model} placeholder={TL_DEFAULT[settings.translation.provider]} />
              <button class="ghost small" title={t("load_models")} onclick={() => loadModels(settings!.translation.provider, "text")}>↻</button>
            </div>
            <datalist id="tl-models">
              {#each models[`${settings.translation.provider}:text`] ?? [] as m}<option value={m}></option>{/each}
            </datalist>
          </label>
          {#if modelsError}<p class="hint err">{modelsError}</p>{/if}
          {#if settings.transcription.captions}
            <label>
              {t("tl_captions")}
              <select bind:value={settings.translation.captions}>
                <option value="original-cc1">{t("tl_cc_orig")}</option>
                <option value="translated-cc1">{t("tl_cc_tr")}</option>
                <option value="translated-only">{t("tl_cc_only")}</option>
              </select>
            </label>
          {/if}
        </fieldset>

        <Posters
          {settings}
          {t}
          {live}
          {apiKeys}
          {videoSources}
          {segments}
          sourceLabel={(id) => (id === "program" ? t("program") : layerLabel({ id, source: id, rect: { x: 0, y: 0, w: 1, h: 1 }, fit: "contain", visible: true }))}
          onaddtoscene={addPosterLayer}
          onerror={(m) => (error = m)}
          onkeys={async () => (apiKeys = await invoke<Record<string, boolean>>("api_key_status"))}
        />
        {/if}
        <div class="cta">
          {#if live}
            <button class="stop" onclick={endLive} disabled={busy}>■ {t("stop")}</button>
          {:else}
            <button class="go" onclick={goLive} disabled={busy}>● {t("go_live")}</button>
          {/if}
        </div>
      </aside>
    </main>
  </div>
{/if}

<style>
  :global(:root) {
    --bg: #0f1115;
    --panel: #171a21;
    --line: #262b36;
    --text: #e7e9ee;
    --muted: #8a92a6;
    --accent: #4f8cff;
    --live: #ff3b4e;
    --ok: #3ccf8e;
    color-scheme: dark;
  }
  :global(body) {
    margin: 0;
    background: var(--bg);
    color: var(--text);
    font: 13px/1.4 -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Ubuntu, sans-serif;
    user-select: none;
  }
  .app { display: flex; flex-direction: column; height: 100vh; }
  header {
    display: flex; align-items: center; gap: 16px;
    padding: 10px 16px; border-bottom: 1px solid var(--line); background: var(--panel);
  }
  .brand { font-weight: 700; font-size: 15px; letter-spacing: 0.3px; }
  .brand span { color: var(--accent); }
  .status {
    display: flex; align-items: center; gap: 8px; margin-left: auto;
    padding: 4px 10px; border-radius: 999px; background: var(--line); color: var(--muted); font-weight: 600;
  }
  .status.preview { color: var(--text); }
  .status.live { background: var(--live); color: #fff; }
  .status .dot { width: 8px; height: 8px; border-radius: 50%; background: #fff; animation: pulse 1.2s infinite; }
  .status .time { font-variant-numeric: tabular-nums; font-weight: 500; }
  @keyframes pulse { 50% { opacity: 0.3; } }
  .lang { display: flex; gap: 2px; }
  .lang button, .seg button {
    background: transparent; border: 1px solid var(--line); color: var(--muted);
    padding: 4px 10px; cursor: pointer; font: inherit;
  }
  .lang button.on, .seg button.on { background: var(--accent); border-color: var(--accent); color: #fff; }

  main { flex: 1; display: flex; min-height: 0; }
  .tabs { display: flex; position: sticky; top: 0; z-index: 2; background: var(--panel); border-bottom: 1px solid var(--line); }
  .tabs button {
    flex: 1; background: transparent; border: 0; border-bottom: 2px solid transparent; color: var(--muted);
    padding: 10px 6px; cursor: pointer; font: inherit; font-weight: 600;
  }
  .tabs button.on { color: var(--text); border-bottom-color: var(--accent); }
  .scenes {
    display: flex; align-items: center; gap: 6px; padding: 8px 16px;
    border-bottom: 1px solid var(--line); background: var(--panel);
  }
  .scenes button {
    background: transparent; border: 1px solid var(--line); color: var(--muted);
    padding: 6px 14px; border-radius: 6px; cursor: pointer; font: inherit;
  }
  .scenes button.on { background: var(--accent); border-color: var(--accent); color: #fff; }
  .scenes small { opacity: 0.7; margin-left: 4px; }
  .onair { margin-left: auto; color: var(--live); font-weight: 600; }
  ul.layers { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 2px; }
  ul.layers li { display: flex; align-items: center; gap: 2px; border-radius: 5px; }
  ul.layers li.sel { background: rgba(79, 140, 255, 0.18); }
  ul.layers button {
    background: transparent; border: 0; color: var(--text); cursor: pointer; font: inherit; padding: 5px 6px;
  }
  ul.layers .name { flex: 1; text-align: left; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  ul.layers .icon { color: var(--muted); font-size: 11px; }
  ul.layers .icon:hover { color: var(--text); }
  .hint { margin: 0; color: var(--muted); font-size: 12px; }
  .props { display: flex; flex-direction: column; gap: 8px; padding-top: 8px; border-top: 1px dashed var(--line); }
  input[type="color"] { width: 40px; height: 24px; border: 0; background: none; padding: 0; }
  input[type="range"] { accent-color: var(--accent); }
  textarea {
    background: var(--bg); color: var(--text); border: 1px solid var(--line); border-radius: 5px;
    padding: 6px 8px; font: inherit; resize: vertical; user-select: text;
  }
  button.on { background: var(--accent); border-color: var(--accent); color: #fff; }
  .ainput { display: flex; flex-direction: column; gap: 4px; }
  .aname { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--text); max-width: 190px; }
  .meter.small { height: 4px; }
  button.muted { border-color: var(--live); }
  .dest { display: flex; flex-direction: column; gap: 6px; padding: 8px; border: 1px solid var(--line); border-radius: 6px; }
  .dest.off { opacity: 0.6; }
  .dname { flex: 1; min-width: 0; font-weight: 600; }
  .chip { font-size: 11px; padding: 2px 6px; border-radius: 999px; background: var(--line); color: var(--muted); white-space: nowrap; }
  .chip.live { background: var(--ok); color: #04210f; }
  .chip.reconnecting, .chip.connecting { background: #ffc53d; color: #2a1e00; }
  .chip.error { background: var(--live); color: #fff; }
  .hint.err { color: #ffb3bb; }
  .transcript { border: 1px solid var(--line); border-radius: 6px; display: flex; flex-direction: column; max-height: 140px; }
  .thead { display: flex; align-items: center; gap: 8px; padding: 6px 10px; border-bottom: 1px solid var(--line); color: var(--muted); }
  .thead .right { margin-left: auto; }
  .tbody { overflow-y: auto; padding: 6px 10px; user-select: text; }
  .tbody p { margin: 0 0 4px; }
  .tbody p.pending { color: var(--muted); font-style: italic; }
  .tbody p.tl { color: var(--accent); margin-top: -2px; }
  .stage { flex: 1; display: flex; flex-direction: column; gap: 10px; padding: 16px; min-width: 0; }
  .meters { display: flex; flex-direction: column; gap: 3px; }
  .meter { height: 5px; background: var(--line); border-radius: 3px; overflow: hidden; }
  .meter .fill {
    height: 100%; transition: width 80ms linear;
    background: linear-gradient(90deg, var(--ok) 0%, var(--ok) 70%, #ffc53d 85%, var(--live) 100%);
    background-size: 100vw 100%;
  }
  .stats { display: flex; flex-wrap: wrap; gap: 14px; color: var(--muted); font-variant-numeric: tabular-nums; }
  .stats .path { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; max-width: 50%; }
  .error {
    display: flex; align-items: center; gap: 8px; padding: 8px 12px;
    border: 1px solid var(--live); border-radius: 6px; color: #ffb3bb; user-select: text;
  }
  .error span { flex: 1; word-break: break-word; }

  aside {
    width: 320px; overflow-y: auto; border-left: 1px solid var(--line); background: var(--panel);
    display: flex; flex-direction: column;
  }
  fieldset { border: 0; border-bottom: 1px solid var(--line); margin: 0; padding: 12px 16px; display: flex; flex-direction: column; gap: 10px; }
  fieldset:disabled { opacity: 0.55; }
  h2 {
    display: flex; align-items: center; justify-content: space-between;
    margin: 0; font-size: 11px; text-transform: uppercase; letter-spacing: 0.8px; color: var(--muted);
  }
  label { display: flex; flex-direction: column; gap: 4px; color: var(--muted); }
  label small.ok { color: var(--ok); }
  .row { display: flex; align-items: center; justify-content: space-between; color: var(--muted); }
  .seg { display: flex; }
  .seg button:first-child { border-radius: 5px 0 0 5px; }
  .seg button:last-child { border-radius: 0 5px 5px 0; }
  .inline { display: flex; gap: 6px; }
  .inline input { flex: 1; min-width: 0; }
  select, input[type="text"], input[type="password"], input[type="number"] {
    background: var(--bg); color: var(--text); border: 1px solid var(--line);
    border-radius: 5px; padding: 6px 8px; font: inherit; user-select: text;
  }
  select:focus, input:focus { outline: 1px solid var(--accent); }
  .switch { accent-color: var(--accent); width: 16px; height: 16px; }
  button.ghost {
    background: transparent; border: 1px solid var(--line); color: var(--text);
    border-radius: 5px; padding: 6px 10px; cursor: pointer; font: inherit;
  }
  button.small { align-self: flex-start; padding: 4px 10px; }
  button:disabled { opacity: 0.5; cursor: default; }
  .cta { padding: 16px; margin-top: auto; }
  .cta button {
    width: 100%; padding: 12px; border: 0; border-radius: 8px; font: inherit; font-weight: 700;
    font-size: 14px; cursor: pointer; color: #fff;
  }
  .go { background: var(--live); }
  .stop { background: #3a4152; }
</style>
