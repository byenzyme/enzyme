// WebGL2 Glow Renderer: two-pass gaussian blur with ping-pong framebuffers (ported from aficionado)

import type { ChunkNode } from "./types";
import { COLORS, pathHue, hslToRgb } from "./types";
import type { CameraState } from "./Camera";

// Shader sources
const QUAD_VS = `#version 300 es
in vec2 a_position;
out vec2 v_texCoord;
void main() {
  gl_Position = vec4(a_position, 0.0, 1.0);
  v_texCoord = a_position * 0.5 + 0.5;
}`;

const BLUR_H_FS = `#version 300 es
precision mediump float;
in vec2 v_texCoord;
out vec4 fragColor;
uniform sampler2D u_texture;
uniform float u_texelSize;
uniform float u_blurRadius;

void main() {
  vec4 sum = vec4(0.0);
  float totalWeight = 0.0;
  float sigma = u_blurRadius * 0.5;
  float sigma2 = 2.0 * sigma * sigma;

  for (float i = -12.0; i <= 12.0; i += 1.0) {
    float offset = i * u_texelSize * (u_blurRadius / 12.0);
    float w = exp(-(i * i) / sigma2);
    sum += texture(u_texture, vec2(v_texCoord.x + offset, v_texCoord.y)) * w;
    totalWeight += w;
  }
  fragColor = sum / totalWeight;
}`;

const BLUR_V_FS = `#version 300 es
precision mediump float;
in vec2 v_texCoord;
out vec4 fragColor;
uniform sampler2D u_texture;
uniform float u_texelSize;
uniform float u_blurRadius;

void main() {
  vec4 sum = vec4(0.0);
  float totalWeight = 0.0;
  float sigma = u_blurRadius * 0.5;
  float sigma2 = 2.0 * sigma * sigma;

  for (float i = -12.0; i <= 12.0; i += 1.0) {
    float offset = i * u_texelSize * (u_blurRadius / 12.0);
    float w = exp(-(i * i) / sigma2);
    sum += texture(u_texture, vec2(v_texCoord.x, v_texCoord.y + offset)) * w;
    totalWeight += w;
  }
  fragColor = sum / totalWeight;
}`;

const SHAPE_VS = `#version 300 es
in vec2 a_position;
uniform mat3 u_transform;
void main() {
  vec3 pos = u_transform * vec3(a_position, 1.0);
  gl_Position = vec4(pos.xy, 0.0, 1.0);
}`;

const SHAPE_FS = `#version 300 es
precision mediump float;
out vec4 fragColor;
uniform vec4 u_color;
void main() {
  fragColor = u_color;
}`;

const PASSTHROUGH_FS = `#version 300 es
precision mediump float;
in vec2 v_texCoord;
out vec4 fragColor;
uniform sampler2D u_texture;
void main() {
  fragColor = texture(u_texture, v_texCoord);
}`;

function createShader(gl: WebGL2RenderingContext, type: number, source: string): WebGLShader | null {
  const shader = gl.createShader(type);
  if (!shader) return null;
  gl.shaderSource(shader, source);
  gl.compileShader(shader);
  if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) {
    console.error("Shader compile error:", gl.getShaderInfoLog(shader));
    gl.deleteShader(shader);
    return null;
  }
  return shader;
}

function createProgram(
  gl: WebGL2RenderingContext,
  vs: string,
  fs: string
): WebGLProgram | null {
  const vertShader = createShader(gl, gl.VERTEX_SHADER, vs);
  const fragShader = createShader(gl, gl.FRAGMENT_SHADER, fs);
  if (!vertShader || !fragShader) return null;

  const program = gl.createProgram();
  if (!program) return null;
  gl.attachShader(program, vertShader);
  gl.attachShader(program, fragShader);
  gl.linkProgram(program);
  if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
    console.error("Program link error:", gl.getProgramInfoLog(program));
    gl.deleteProgram(program);
    return null;
  }
  return program;
}

export class GlowRenderer {
  private gl: WebGL2RenderingContext | null = null;
  private canvas: HTMLCanvasElement | null = null;

  // Programs
  private shapeProgram: WebGLProgram | null = null;
  private blurHProgram: WebGLProgram | null = null;
  private blurVProgram: WebGLProgram | null = null;
  private passthroughProgram: WebGLProgram | null = null;

  // Buffers
  private quadVAO: WebGLVertexArrayObject | null = null;
  private shapeVAO: WebGLVertexArrayObject | null = null;
  private shapeVBO: WebGLBuffer | null = null;

  // Framebuffers for ping-pong blur
  private fbA: WebGLFramebuffer | null = null;
  private fbB: WebGLFramebuffer | null = null;
  private texA: WebGLTexture | null = null;
  private texB: WebGLTexture | null = null;
  private fbWidth = 0;
  private fbHeight = 0;

  private supported = false;

  init(canvas: HTMLCanvasElement): boolean {
    const gl = canvas.getContext("webgl2", {
      alpha: true,
      premultipliedAlpha: true,
      antialias: false,
    });
    if (!gl) return false;

    this.gl = gl;
    this.canvas = canvas;

    // Create programs
    this.shapeProgram = createProgram(gl, SHAPE_VS, SHAPE_FS);
    this.blurHProgram = createProgram(gl, QUAD_VS, BLUR_H_FS);
    this.blurVProgram = createProgram(gl, QUAD_VS, BLUR_V_FS);
    this.passthroughProgram = createProgram(gl, QUAD_VS, PASSTHROUGH_FS);

    if (
      !this.shapeProgram ||
      !this.blurHProgram ||
      !this.blurVProgram ||
      !this.passthroughProgram
    ) {
      return false;
    }

    // Full-screen quad VAO
    const quadVerts = new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]);
    this.quadVAO = gl.createVertexArray();
    gl.bindVertexArray(this.quadVAO);
    const quadBuf = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, quadBuf);
    gl.bufferData(gl.ARRAY_BUFFER, quadVerts, gl.STATIC_DRAW);
    gl.enableVertexAttribArray(0);
    gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 0, 0);
    gl.bindVertexArray(null);

    // Shape VAO (dynamic)
    this.shapeVAO = gl.createVertexArray();
    this.shapeVBO = gl.createBuffer();
    gl.bindVertexArray(this.shapeVAO);
    gl.bindBuffer(gl.ARRAY_BUFFER, this.shapeVBO);
    gl.enableVertexAttribArray(0);
    gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 0, 0);
    gl.bindVertexArray(null);

    this.supported = true;
    return true;
  }

  isSupported(): boolean {
    return this.supported;
  }

  private ensureFramebuffers(width: number, height: number): void {
    const gl = this.gl!;
    if (this.fbWidth === width && this.fbHeight === height) return;

    // Clean up old
    if (this.fbA) gl.deleteFramebuffer(this.fbA);
    if (this.fbB) gl.deleteFramebuffer(this.fbB);
    if (this.texA) gl.deleteTexture(this.texA);
    if (this.texB) gl.deleteTexture(this.texB);

    const createFB = () => {
      const tex = gl.createTexture()!;
      gl.bindTexture(gl.TEXTURE_2D, tex);
      gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA8, width, height, 0, gl.RGBA, gl.UNSIGNED_BYTE, null);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);

      const fb = gl.createFramebuffer()!;
      gl.bindFramebuffer(gl.FRAMEBUFFER, fb);
      gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, tex, 0);

      return { fb, tex };
    };

    const a = createFB();
    const b = createFB();
    this.fbA = a.fb;
    this.texA = a.tex;
    this.fbB = b.fb;
    this.texB = b.tex;
    this.fbWidth = width;
    this.fbHeight = height;

    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
  }

  // Generate rounded-rect vertices
  private roundedRectVertices(
    x: number,
    y: number,
    w: number,
    h: number,
    r: number,
    padding: number
  ): Float32Array {
    const px = x - padding;
    const py = y - padding;
    const pw = w + padding * 2;
    const ph = h + padding * 2;
    const cr = Math.min(r + padding, Math.min(pw, ph) / 2);

    const verts: number[] = [];
    const cx = px + pw / 2;
    const cy = py + ph / 2;

    // Build outline vertices and fan-fill from center
    const segments = 8;
    const corners = [
      { cx: px + cr, cy: py + cr, startAngle: Math.PI, endAngle: Math.PI * 1.5 },
      { cx: px + pw - cr, cy: py + cr, startAngle: Math.PI * 1.5, endAngle: Math.PI * 2 },
      { cx: px + pw - cr, cy: py + ph - cr, startAngle: 0, endAngle: Math.PI * 0.5 },
      { cx: px + cr, cy: py + ph - cr, startAngle: Math.PI * 0.5, endAngle: Math.PI },
    ];

    const outline: number[] = [];
    for (const corner of corners) {
      for (let i = 0; i <= segments; i++) {
        const angle = corner.startAngle + ((corner.endAngle - corner.startAngle) * i) / segments;
        outline.push(corner.cx + Math.cos(angle) * cr, corner.cy + Math.sin(angle) * cr);
      }
    }

    // Fan triangles from center
    for (let i = 0; i < outline.length / 2; i++) {
      const j = (i + 1) % (outline.length / 2);
      verts.push(cx, cy);
      verts.push(outline[i * 2], outline[i * 2 + 1]);
      verts.push(outline[j * 2], outline[j * 2 + 1]);
    }

    return new Float32Array(verts);
  }

  render(
    nodes: ChunkNode[],
    camera: CameraState,
    canvasWidth: number,
    canvasHeight: number
  ): void {
    if (!this.gl || !this.supported) return;

    const gl = this.gl;
    const dpr = window.devicePixelRatio || 1;
    const width = Math.floor(canvasWidth * dpr);
    const height = Math.floor(canvasHeight * dpr);

    // Only render highlighted nodes
    const glowNodes = nodes.filter((n) => n.hasSearchResults || n.isRelevant);
    if (glowNodes.length === 0) {
      // Clear the canvas
      gl.viewport(0, 0, this.canvas!.width, this.canvas!.height);
      gl.clearColor(0, 0, 0, 0);
      gl.clear(gl.COLOR_BUFFER_BIT);
      return;
    }

    this.ensureFramebuffers(width, height);

    // Step 1: Render shapes to framebuffer A
    gl.bindFramebuffer(gl.FRAMEBUFFER, this.fbA);
    gl.viewport(0, 0, width, height);
    gl.clearColor(0, 0, 0, 0);
    gl.clear(gl.COLOR_BUFFER_BIT);
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.SRC_ALPHA, gl.ONE);

    gl.useProgram(this.shapeProgram);
    const transformLoc = gl.getUniformLocation(this.shapeProgram!, "u_transform");
    const colorLoc = gl.getUniformLocation(this.shapeProgram!, "u_color");

    const rawZoom = camera.zoom;
    const glowPadding = rawZoom < 0.3 ? 20 / rawZoom : 40;

    for (const node of glowNodes) {
      // World-to-NDC transform
      const sx = (2 * camera.zoom) / canvasWidth;
      const sy = (-2 * camera.zoom) / canvasHeight;
      const tx = (2 * (camera.x + node.x * camera.zoom)) / canvasWidth - 1;
      const ty = 1 - (2 * (camera.y + node.y * camera.zoom)) / canvasHeight;

      // Column-major 3x3 matrix
      gl.uniformMatrix3fv(transformLoc, false, [sx, sy, 0, 0, 0, 0, tx, ty, 1]);

      // Pick glow color — per-cluster hue for relevant, fixed blue for search
      const glowColor = node.isRelevant
        ? hslToRgb(pathHue(node.filePath), 70, 50)
        : COLORS.searchGlow;
      const intensity = node.isRelevant ? 0.8 : 0.6;
      gl.uniform4f(
        colorLoc,
        (glowColor.r / 255) * intensity,
        (glowColor.g / 255) * intensity,
        (glowColor.b / 255) * intensity,
        intensity
      );

      const verts = this.roundedRectVertices(
        node.x,
        node.y,
        node.width,
        node.height,
        8,
        glowPadding
      );

      gl.bindVertexArray(this.shapeVAO);
      gl.bindBuffer(gl.ARRAY_BUFFER, this.shapeVBO);
      gl.bufferData(gl.ARRAY_BUFFER, verts, gl.DYNAMIC_DRAW);
      gl.drawArrays(gl.TRIANGLES, 0, verts.length / 2);
    }

    // Step 2: Multi-pass gaussian blur (ping-pong)
    const blurRadius = Math.min(60, rawZoom < 0.3 ? 30 * (0.5 / rawZoom) : 30);
    const BLUR_PASSES = 3;

    gl.disable(gl.BLEND);

    for (let pass = 0; pass < BLUR_PASSES; pass++) {
      // Horizontal: A → B
      gl.bindFramebuffer(gl.FRAMEBUFFER, this.fbB);
      gl.viewport(0, 0, width, height);
      gl.clearColor(0, 0, 0, 0);
      gl.clear(gl.COLOR_BUFFER_BIT);
      gl.useProgram(this.blurHProgram);
      gl.activeTexture(gl.TEXTURE0);
      gl.bindTexture(gl.TEXTURE_2D, this.texA);
      gl.uniform1i(gl.getUniformLocation(this.blurHProgram!, "u_texture"), 0);
      gl.uniform1f(gl.getUniformLocation(this.blurHProgram!, "u_texelSize"), 1 / width);
      gl.uniform1f(gl.getUniformLocation(this.blurHProgram!, "u_blurRadius"), blurRadius);
      gl.bindVertexArray(this.quadVAO);
      gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);

      // Vertical: B → A
      gl.bindFramebuffer(gl.FRAMEBUFFER, this.fbA);
      gl.viewport(0, 0, width, height);
      gl.clearColor(0, 0, 0, 0);
      gl.clear(gl.COLOR_BUFFER_BIT);
      gl.useProgram(this.blurVProgram);
      gl.activeTexture(gl.TEXTURE0);
      gl.bindTexture(gl.TEXTURE_2D, this.texB);
      gl.uniform1i(gl.getUniformLocation(this.blurVProgram!, "u_texture"), 0);
      gl.uniform1f(gl.getUniformLocation(this.blurVProgram!, "u_texelSize"), 1 / height);
      gl.uniform1f(gl.getUniformLocation(this.blurVProgram!, "u_blurRadius"), blurRadius);
      gl.bindVertexArray(this.quadVAO);
      gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
    }

    // Step 3: Composite to screen
    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    gl.viewport(0, 0, this.canvas!.width, this.canvas!.height);
    gl.clearColor(0, 0, 0, 0);
    gl.clear(gl.COLOR_BUFFER_BIT);
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.ONE, gl.ONE_MINUS_SRC_ALPHA);
    gl.useProgram(this.passthroughProgram);
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, this.texA);
    gl.uniform1i(gl.getUniformLocation(this.passthroughProgram!, "u_texture"), 0);
    gl.bindVertexArray(this.quadVAO);
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
  }

  resize(width: number, height: number): void {
    if (!this.canvas) return;
    const dpr = window.devicePixelRatio || 1;
    this.canvas.width = Math.floor(width * dpr);
    this.canvas.height = Math.floor(height * dpr);
    this.canvas.style.width = `${width}px`;
    this.canvas.style.height = `${height}px`;
    // Force framebuffer recreation
    this.fbWidth = 0;
    this.fbHeight = 0;
  }

  destroy(): void {
    const gl = this.gl;
    if (!gl) return;

    if (this.fbA) gl.deleteFramebuffer(this.fbA);
    if (this.fbB) gl.deleteFramebuffer(this.fbB);
    if (this.texA) gl.deleteTexture(this.texA);
    if (this.texB) gl.deleteTexture(this.texB);
    if (this.shapeProgram) gl.deleteProgram(this.shapeProgram);
    if (this.blurHProgram) gl.deleteProgram(this.blurHProgram);
    if (this.blurVProgram) gl.deleteProgram(this.blurVProgram);
    if (this.passthroughProgram) gl.deleteProgram(this.passthroughProgram);

    this.gl = null;
    this.canvas = null;
    this.supported = false;
  }
}
