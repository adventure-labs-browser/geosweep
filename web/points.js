/* GeoSweep's native WebGL point renderer.
   Coordinates are packed into 12 bytes per point: WebMercator float32 x/y,
   uint16 cache type, uint8 difficulty*2, uint8 terrain*2.
   No GeoJSON tiling or MapLibre clustering in Points mode. */
"use strict";

const GeoSweepPoints = {
  id: "geosweep-gpu-points",
  type: "custom",
  renderingMode: "2d",
  visible: false,
  scope: "local",
  globalCount: 0,
  localCount: 0,
  globalReady: false,
  typeFilter: 0,
  difficultyFilter: 0,
  terrainFilter: 0,
  _globalPending: null,
  _localPending: null,

  mercator(lon, lat) {
    const clamped = Math.min(85.051129, Math.max(-85.051129, lat));
    const phi = clamped * Math.PI / 180;
    return [(lon + 180) / 360,
      (1 - Math.log(Math.tan(Math.PI / 4 + phi / 2)) / Math.PI) / 2];
  },

  pack(rows) {
    const packed = new ArrayBuffer(rows.length * 12);
    const view = new DataView(packed);
    let n = 0;
    for (const r of rows) {
      if (r[2] == null || r[3] == null) continue;
      const [x, y] = this.mercator(r[3], r[2]);
      if (!Number.isFinite(x) || !Number.isFinite(y)) continue;
      const p = n++ * 12;
      view.setFloat32(p, x, true);
      view.setFloat32(p + 4, y, true);
      view.setUint16(p + 8, r[4] || 0, true);
      view.setUint8(p + 10, r[5] == null ? 0 : Math.round(r[5] * 2));
      view.setUint8(p + 11, r[6] == null ? 0 : Math.round(r[6] * 2));
    }
    return new Uint8Array(packed, 0, n * 12);
  },

  setFilters(type, difficulty, terrain) {
    this.typeFilter = type || 0;
    this.difficultyFilter = difficulty || 0;
    this.terrainFilter = terrain || 0;
    this.map?.triggerRepaint();
  },

  setLocal(rows) {
    this._localPending = this.pack(rows);
    this.localCount = this._localPending.byteLength / 12;
    if (this.gl) this._upload("localBuffer", this._localPending);
    this.map?.triggerRepaint();
  },

  setGlobal(data) {
    const bytes = data instanceof Uint8Array ? data : new Uint8Array(data);
    if (bytes.byteLength % 12 !== 0)
      throw new Error("Invalid binary points file size");
    this._globalPending = bytes;
    this.globalCount = bytes.byteLength / 12;
    this.globalReady = true;
    if (this.gl) this._upload("globalBuffer", bytes);
    this.map?.triggerRepaint();
  },

  _upload(bufferName, bytes) {
    const gl = this.gl;
    gl.bindBuffer(gl.ARRAY_BUFFER, this[bufferName]);
    gl.bufferData(gl.ARRAY_BUFFER, bytes, gl.STATIC_DRAW);
    if (bufferName === "localBuffer") this._localPending = null;
    if (bufferName === "globalBuffer") this._globalPending = null;
  },

  onAdd(map, gl) {
    this.map = map;
    this.gl = gl;
    const vert = [
      "precision highp float;",
      "attribute vec2 a_pos;",
      "attribute float a_type, a_difficulty, a_terrain;",
      "uniform mat4 u_matrix;",
      "uniform float u_type, u_difficulty, u_terrain, u_size;",
      "void main() {",
      "  bool hidden = (u_type > 0.0 && abs(a_type - u_type) > 0.5)",
      "    || (u_difficulty > 0.0 && a_difficulty < u_difficulty)",
      "    || (u_terrain > 0.0 && a_terrain < u_terrain);",
      "  if (hidden) { gl_Position = vec4(-2.0, -2.0, 0.0, 1.0); gl_PointSize = 0.0; return; }",
      "  gl_Position = u_matrix * vec4(a_pos, 0.0, 1.0);",
      "  gl_PointSize = u_size;",
      "}"
    ].join("\n");
    const frag = [
      "precision mediump float;",
      "void main() {",
      "  float d = length(gl_PointCoord - vec2(0.5));",
      "  if (d > 0.5) discard;",
      "  gl_FragColor = d > 0.39 ? vec4(0.051, 0.067, 0.090, 1.0)",
      "                              : vec4(0.345, 0.651, 1.0, 1.0);",
      "}"
    ].join("\n");
    const compile = (kind, source) => {
      const shader = gl.createShader(kind);
      gl.shaderSource(shader, source);
      gl.compileShader(shader);
      if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS))
        throw new Error("Point shader: " + gl.getShaderInfoLog(shader));
      return shader;
    };
    const program = gl.createProgram();
    const vs = compile(gl.VERTEX_SHADER, vert);
    const fs = compile(gl.FRAGMENT_SHADER, frag);
    gl.attachShader(program, vs);
    gl.attachShader(program, fs);
    gl.linkProgram(program);
    gl.deleteShader(vs);
    gl.deleteShader(fs);
    if (!gl.getProgramParameter(program, gl.LINK_STATUS))
      throw new Error("Point program: " + gl.getProgramInfoLog(program));
    this.program = program;
    this.globalBuffer = gl.createBuffer();
    this.localBuffer = gl.createBuffer();
    this.aPos = gl.getAttribLocation(program, "a_pos");
    this.aType = gl.getAttribLocation(program, "a_type");
    this.aDifficulty = gl.getAttribLocation(program, "a_difficulty");
    this.aTerrain = gl.getAttribLocation(program, "a_terrain");
    this.uMatrix = gl.getUniformLocation(program, "u_matrix");
    this.uType = gl.getUniformLocation(program, "u_type");
    this.uDifficulty = gl.getUniformLocation(program, "u_difficulty");
    this.uTerrain = gl.getUniformLocation(program, "u_terrain");
    this.uSize = gl.getUniformLocation(program, "u_size");
    if (this._globalPending) this._upload("globalBuffer", this._globalPending);
    if (this._localPending) this._upload("localBuffer", this._localPending);
  },

  render(gl, matrix) {
    const count = this.scope === "global" ? this.globalCount : this.localCount;
    if (!this.visible || !count || !this.program) return;
    const originalBlend = gl.isEnabled(gl.BLEND);
    const originalDepth = gl.isEnabled(gl.DEPTH_TEST);
    gl.useProgram(this.program);
    gl.bindBuffer(gl.ARRAY_BUFFER,
      this.scope === "global" ? this.globalBuffer : this.localBuffer);
    for (const [loc, size, kind, offset] of [
      [this.aPos, 2, gl.FLOAT, 0],
      [this.aType, 1, gl.UNSIGNED_SHORT, 8],
      [this.aDifficulty, 1, gl.UNSIGNED_BYTE, 10],
      [this.aTerrain, 1, gl.UNSIGNED_BYTE, 11]
    ]) {
      gl.enableVertexAttribArray(loc);
      gl.vertexAttribPointer(loc, size, kind, false, 12, offset);
    }
    gl.uniformMatrix4fv(this.uMatrix, false, matrix);
    gl.uniform1f(this.uType, this.typeFilter);
    gl.uniform1f(this.uDifficulty, this.difficultyFilter * 2);
    gl.uniform1f(this.uTerrain, this.terrainFilter * 2);
    const z = this.map.getZoom();
    gl.uniform1f(this.uSize, z < 5 ? 3 : z > 12 ? 8 : 3 + (z - 5) * 5 / 7);
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);
    gl.disable(gl.DEPTH_TEST);
    gl.drawArrays(gl.POINTS, 0, count);
    if (!originalBlend) gl.disable(gl.BLEND);
    if (originalDepth) gl.enable(gl.DEPTH_TEST);
  },

  onRemove(_map, gl) {
    if (this.globalBuffer) gl.deleteBuffer(this.globalBuffer);
    if (this.localBuffer) gl.deleteBuffer(this.localBuffer);
    if (this.program) gl.deleteProgram(this.program);
    this.gl = null;
  }
};
