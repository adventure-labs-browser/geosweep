import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";
import { test } from "node:test";

const source = readFileSync(new URL("../web/points.js", import.meta.url), "utf8");
const scope = { ArrayBuffer, DataView, Uint8Array, Math };
runInNewContext(source + "\nthis.renderer = GeoSweepPoints;", scope);
const renderer = scope.renderer;

test("packs coordinates and geocache filters into 12 bytes", () => {
  const r = ["GC1", "Cache", 45, -90, 8, 2.5, 3, null, 0, 5];
  const payload = renderer.pack([r]);
  assert.equal(payload.byteLength, 12);
  const view = new DataView(payload.buffer, payload.byteOffset);
  assert.ok(Math.abs(view.getFloat32(0, true) - 0.25) < 1e-6);
  assert.ok(Math.abs(view.getFloat32(4, true) - renderer.mercator(-90, 45)[1]) < 1e-6);
  assert.equal(view.getUint16(8, true), 8);
  assert.equal(view.getUint8(10), 5);
  assert.equal(view.getUint8(11), 6);
});

test("omits coordinate-less records and validates global binary data", () => {
  const row = ["GC2", "Premium", null, null, 2, 2, 2];
  assert.equal(renderer.pack([row]).byteLength, 0);
  assert.throws(() => renderer.setGlobal(new ArrayBuffer(13)), /Invalid binary/);
  renderer.setGlobal(new ArrayBuffer(24));
  assert.equal(renderer.globalCount, 2);
  assert.equal(renderer.globalReady, true);
});

test("zooming out in Points mode never displays cluster or density layers", () => {
  const app = readFileSync(new URL("../web/app.js", import.meta.url), "utf8");
  const layers = new Map();
  let geojsonUpdates = 0;
  const m = {
    getZoom: () => 2,
    getSource: () => ({setData() {geojsonUpdates++;}}),
    setLayoutProperty(id, property, value) {layers.set(id, value);},
    triggerRepaint() {}
  };
  const pointLayer = {
    globalReady: true, globalComplete: true,
    visible: false, scope: "local",
    setFilters() {}, setLocal() {}
  };
  const sandbox = {
    console, GeoSweepPoints: pointLayer,
    document: {getElementById: () => ({value:""})}, fakeMap: m
  };
  runInNewContext(app.replace(/\nboot\(\);\s*$/, "") +
    '\nmap = fakeMap; mode = "all"; updateMap([], true);', sandbox);
  assert.equal(pointLayer.visible, true);
  assert.equal(pointLayer.scope, "global");
  assert.equal(layers.get("density-circles"), "none");
  assert.equal(layers.get("density-count"), "none");
  assert.equal(layers.get("clusters"), "none");
  assert.equal(layers.get("points"), "none");
  assert.equal(geojsonUpdates, 0);
});
