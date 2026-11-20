<script lang="ts">
  import type { Layer, Rect } from "$lib/types";

  interface Props {
    frame: string | null;
    canvas: { w: number; h: number };
    layers: Layer[];
    selectedId: string | null;
    labelOf: (l: Layer) => string;
    onselect: (id: string | null) => void;
    /** Se llama mientras se arrastra (cambios en vivo). */
    onrect: (id: string, rect: Rect) => void;
    emptyText: string;
    /** Vista previa nativa (GPU) detrás del webview: aquí solo queda un hueco transparente. */
    native?: boolean;
    /** Hay pipeline corriendo (vista previa o en vivo). */
    active?: boolean;
    /** Rectángulo del lienzo en px CSS de la ventana, o null para ocultar el video nativo. */
    onviewport?: (r: { x: number; y: number; w: number; h: number; pageH: number } | null) => void;
  }
  let {
    frame,
    canvas,
    layers,
    selectedId,
    labelOf,
    onselect,
    onrect,
    emptyText,
    native = false,
    active = false,
    onviewport,
  }: Props = $props();

  let canvasEl = $state<HTMLDivElement | undefined>();

  // Mantiene la vista nativa exactamente sobre el hueco del lienzo.
  $effect(() => {
    if (!native || !canvasEl || !onviewport) return;
    if (!active) {
      onviewport(null);
      return;
    }
    let last = "";
    const report = () => {
      const r = canvasEl!.getBoundingClientRect();
      const rect = {
        x: Math.round(r.left),
        y: Math.round(r.top),
        w: Math.round(r.width),
        h: Math.round(r.height),
        pageH: window.innerHeight,
      };
      const key = JSON.stringify(rect);
      if (key !== last) {
        last = key;
        onviewport(rect.w > 0 && rect.h > 0 ? rect : null);
      }
    };
    report();
    const ro = new ResizeObserver(report);
    ro.observe(canvasEl);
    window.addEventListener("resize", report);
    // Cambios de posición sin cambio de tamaño (paneles que aparecen arriba).
    const timer = setInterval(report, 500);
    return () => {
      ro.disconnect();
      window.removeEventListener("resize", report);
      clearInterval(timer);
      onviewport(null);
    };
  });

  let wrap: HTMLDivElement;
  let box = $state({ w: 0, h: 0 });

  // Ajusta el lienzo al espacio disponible manteniendo su aspecto.
  $effect(() => {
    const ro = new ResizeObserver(() => {
      const r = wrap.getBoundingClientRect();
      const scale = Math.min(r.width / canvas.w, r.height / canvas.h);
      box = { w: Math.floor(canvas.w * scale), h: Math.floor(canvas.h * scale) };
    });
    ro.observe(wrap);
    return () => ro.disconnect();
  });

  type Handle = "move" | "nw" | "ne" | "sw" | "se";
  let drag: { id: string; handle: Handle; start: Rect; px: number; py: number; aspect: number } | null = null;

  const SNAP = 0.015;
  const MIN = 0.03;

  function snap(v: number, targets: number[]) {
    for (const t of targets) if (Math.abs(v - t) < SNAP) return t;
    return v;
  }

  function down(e: PointerEvent, layer: Layer, handle: Handle) {
    e.stopPropagation();
    e.preventDefault();
    onselect(layer.id);
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    drag = {
      id: layer.id,
      handle,
      start: { ...layer.rect },
      px: e.clientX,
      py: e.clientY,
      aspect: layer.rect.w / layer.rect.h,
    };
  }

  function move(e: PointerEvent) {
    if (!drag || box.w === 0) return;
    const dx = (e.clientX - drag.px) / box.w;
    const dy = (e.clientY - drag.py) / box.h;
    const s = drag.start;
    let r: Rect = { ...s };
    if (drag.handle === "move") {
      r.x = s.x + dx;
      r.y = s.y + dy;
      // Imantar bordes y centro al lienzo.
      r.x = snap(r.x, [0, 0.5 - r.w / 2, 1 - r.w]);
      r.y = snap(r.y, [0, 0.5 - r.h / 2, 1 - r.h]);
    } else {
      let left = s.x, top = s.y, right = s.x + s.w, bottom = s.y + s.h;
      if (drag.handle.includes("w")) left = snap(Math.min(s.x + dx, right - MIN), [0, 0.5]);
      if (drag.handle.includes("e")) right = snap(Math.max(s.x + s.w + dx, left + MIN), [1, 0.5]);
      if (drag.handle.includes("n")) top = snap(Math.min(s.y + dy, bottom - MIN), [0, 0.5]);
      if (drag.handle.includes("s")) bottom = snap(Math.max(s.y + s.h + dy, top + MIN), [1, 0.5]);
      r = { x: left, y: top, w: right - left, h: bottom - top };
      // Shift: conservar proporción.
      if (e.shiftKey) {
        const h = r.w / drag.aspect;
        r.h = h;
        if (drag.handle.includes("n")) r.y = bottom - h;
      }
    }
    onrect(drag.id, r);
  }

  function up() {
    drag = null;
  }

  const selected = $derived(layers.find((l) => l.id === selectedId) ?? null);
</script>

<div class="wrap" bind:this={wrap}>
  <div
    class="canvas"
    class:native
    role="application"
    bind:this={canvasEl}
    style="width:{box.w}px;height:{box.h}px"
    onpointerdown={() => onselect(null)}
  >
    {#if native}
      {#if !active}<div class="empty">{emptyText}</div>{/if}
    {:else if frame}
      <img src={frame} alt="preview" draggable="false" />
    {:else}
      <div class="empty">{emptyText}</div>
    {/if}
    <div class="overlay" role="presentation" onpointermove={move} onpointerup={up} onpointercancel={up}>
      <!-- Mismo orden que el apilado: la última capa queda encima y recibe el clic. -->
      {#each layers as layer, i (layer.id)}
        {@const r = layer.rect}
        <div
          class="layer"
          class:hidden={!layer.visible}
          style="left:{r.x * 100}%;top:{r.y * 100}%;width:{r.w * 100}%;height:{r.h * 100}%;z-index:{i + 1}"
          onpointerdown={(e) => down(e, layer, "move")}
          role="button"
          tabindex="-1"
        ></div>
      {/each}
      <!-- Marco de la capa seleccionada: siempre visible encima; solo las
           esquinas reciben clics, así no tapa a las capas superiores. -->
      {#if selected}
        {@const r = selected.rect}
        <div class="frame" style="left:{r.x * 100}%;top:{r.y * 100}%;width:{r.w * 100}%;height:{r.h * 100}%">
          <span class="tag">{labelOf(selected)}</span>
          {#each ["nw", "ne", "sw", "se"] as const as hd}
            <span class="handle {hd}" onpointerdown={(e) => down(e, selected, hd)} role="presentation"></span>
          {/each}
        </div>
      {/if}
    </div>
  </div>
</div>

<style>
  .wrap {
    flex: 1;
    min-height: 0;
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .canvas {
    position: relative;
    background: #000;
  }
  /* Hueco transparente: el video lo dibuja la GPU en una vista nativa debajo. */
  .canvas.native {
    background: transparent;
    border-radius: 6px;
    overflow: hidden;
  }
  img {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    pointer-events: none;
  }
  .empty {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    color: var(--muted);
  }
  .overlay {
    position: absolute;
    inset: 0;
  }
  .layer {
    position: absolute;
    box-sizing: border-box;
    cursor: move;
    outline: 1px dashed transparent;
  }
  .layer:hover {
    outline-color: rgba(255, 255, 255, 0.35);
  }
  .layer.hidden {
    pointer-events: none;
  }
  .frame {
    position: absolute;
    box-sizing: border-box;
    outline: 2px solid var(--accent);
    pointer-events: none;
    z-index: 10000;
  }
  .frame .handle {
    pointer-events: auto;
  }
  .tag {
    position: absolute;
    left: 0;
    top: 0;
    transform: translateY(-100%);
    background: var(--accent);
    color: #fff;
    font-size: 11px;
    padding: 1px 6px;
    white-space: nowrap;
    max-width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .frame .tag {
    transform: none;
  }
  .handle {
    position: absolute;
    width: 12px;
    height: 12px;
    background: #fff;
    border: 2px solid var(--accent);
    border-radius: 2px;
  }
  .nw { left: -6px; top: -6px; cursor: nwse-resize; }
  .ne { right: -6px; top: -6px; cursor: nesw-resize; }
  .sw { left: -6px; bottom: -6px; cursor: nesw-resize; }
  .se { right: -6px; bottom: -6px; cursor: nwse-resize; }
</style>
