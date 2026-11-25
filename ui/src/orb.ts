// The orb: a WebGL2 plasma sphere and the app's connect button.
// A soft luminous rim, silky light flowing through the ball,
// a soft halo. Everything is analytic (sines and exponentials, no noise
// textures, no loops), so one full-canvas pass is cheap even on integrated
// GPUs. The look is driven by a few smoothed uniforms set from the app state.

const VERT = `#version 300 es
in vec2 p;
void main() { gl_Position = vec4(p, 0.0, 1.0); }`;

const FRAG = `#version 300 es
precision highp float;
uniform vec2 uRes;
uniform float uTime, uEnergy, uSpin, uWarm, uHover, uPress, uRipple, uRadius, uLight;
out vec4 outColor;

const float PI = 3.14159265;
const float TAU = 6.2831853;

float hash(vec2 p) {
  p = fract(p * vec2(443.897, 441.423));
  p += dot(p, p.yx + 19.19);
  return fract((p.x + p.y) * p.x);
}

// A slowly flowing field of silk, defined on the 3D surface of the ball, so it
// has no centre and no seams: two passes of domain warping with plain sines.
vec3 silk(vec3 n, float t) {
  vec3 w = n * 1.75;
  w += 0.60 * sin(w.yzx * 1.65 + vec3(t * 0.31, t * 0.23 + 1.7, t * 0.27 + 4.1));
  w += 0.38 * sin(w.zxy * 2.15 + vec3(t * 0.19 + 2.3, t * 0.29 + 0.6, t * 0.21 + 5.2));
  return vec3(sin(w.x * 1.30 + w.y * 0.70 + t * 0.20),
              sin(w.y * 1.10 - w.z * 0.90 - t * 0.17 + 2.0),
              sin(w.z * 1.20 + w.x * 0.80 + t * 0.13 + 4.0));
}

void main() {
  float half_ = 0.5 * min(uRes.x, uRes.y);
  vec2 uv = (gl_FragCoord.xy - 0.5 * uRes) / half_;
  float E = uEnergy, t = uTime;

  // palette: pink / blue / violet, sliding to ember / amber / red when warm
  vec3 cA = mix(vec3(1.00, 0.26, 0.62), vec3(1.00, 0.36, 0.10), uWarm);
  vec3 cB = mix(vec3(0.24, 0.44, 1.00), vec3(1.00, 0.66, 0.24), uWarm);
  vec3 cC = mix(vec3(0.58, 0.34, 1.00), vec3(1.00, 0.20, 0.22), uWarm);

  float R = uRadius * (1.0 + 0.010 * sin(t * 0.9) * E + 0.018 * uHover - 0.030 * uPress);
  vec2 p = uv / R;
  float r = length(p);
  float a = atan(p.y, p.x);
  float px = 1.0 / (half_ * R);                       // one device pixel, in sphere radii
  float inside = 1.0 - smoothstep(1.0 - 6.0 * px, 1.0 + 1.5 * px, r);   // feathered, never a cut edge

  // ---- plasma: silk on a slowly turning ball
  float z = sqrt(max(1.0 - r * r, 0.0));
  vec3 n = vec3(p, z * 0.85);                         // a little depth, as if seen through glass
  float ry = t * 0.11, rx = 0.35 + 0.25 * sin(t * 0.07);
  n.xz = mat2(cos(ry), -sin(ry), sin(ry), cos(ry)) * n.xz;
  n.yz = mat2(cos(rx), -sin(rx), sin(rx), cos(rx)) * n.yz;
  vec3 f = silk(n, t);
  vec3 g = 0.5 + 0.5 * f;
  g = g * g * g;                                      // deep shadows between the folds
  vec3 plasma = cB * g.x + cA * g.y + cC * 0.75 * g.z;
  // never empty: pockets where every fold is low fill with a soft violet
  float low = 1.0 - smoothstep(0.0, 0.30, g.x + g.y + g.z);
  plasma += mix(cC, cB, 0.5 + 0.5 * f.z) * (0.14 + 0.30 * low);
  // soft sheen along the folds, where the field passes through zero
  vec3 fold = exp(-f * f / 0.10);
  plasma += vec3(1.0, 0.94, 1.0) * (fold.x * g.y + fold.y * g.x) * 0.30;
  // the ball glows more toward its rim, like light caught in glass
  float shell = 0.62 + 0.38 * (1.0 - z) * (1.0 - z);
  vec3 col = plasma * shell * inside * E * 0.85;
  float reach = 1.0;

  // ---- rim: two slow counter-rotating lobes of colour, plus a comet while spinning
  float l1 = 0.5 + 0.5 * cos(a - t * 0.21 - 2.3);
  float l2 = 0.5 + 0.5 * cos(a + t * 0.16 + 0.5);
  l1 *= l1 * l1; l2 *= l2 * l2;
  float comet = pow(0.5 + 0.5 * cos(a + t * 1.0), 2.0);
  float lobes = mix(l1 + l2, 0.15 + 1.9 * comet, uSpin * 0.85);
  vec3 rimCol = (cA * l1 + cB * l2 + cC * 0.22) / (l1 + l2 + 0.22);
  rimCol = mix(rimCol, mix(cB, cA, 0.5 + 0.5 * sin(a + t * 0.5)), uSpin * 0.5);

  // inner limb glow: where the sheets meet the glass
  float limb = exp((r - 1.0) * mix(18.0, 5.0, E)) * inside;
  col += rimCol * limb * (0.05 + 0.75 * E) * (0.08 + lobes);
  col += plasma * limb * E * 0.9;

  // the rim line itself
  float d = (r - 1.0) / px;
  float wl = mix(2.4, 3.4, E) * uRes.y / 760.0 + 0.6;
  float line = exp(-d * d / (wl * wl));
  // a prism's signature: the rim splits slightly, red outward and blue inward
  float ca = (0.4 + 0.6 * E) * uRes.y / 760.0;
  vec3 split = vec3(exp(-(d - ca) * (d - ca) / (wl * wl)), line, exp(-(d + ca) * (d + ca) / (wl * wl)));
  float rimI = 0.45 * mix(0.26 + 0.55 * (l1 + l2), 0.32 + 1.30 * lobes, smoothstep(0.0, 0.3, E));
  col += mix(rimCol, vec3(1.0), 0.10 + 0.15 * E * lobes) * split * rimI * (1.0 + 0.5 * uHover);
  // ribbons light the glass where they touch it
  float touch = dot(plasma, vec3(0.34)) * reach * E;
  col += (plasma * 0.6 + vec3(touch * 0.5)) * line * 0.6;

  // outer halo
  float o = max(r - 1.0, 0.0);
  float outside = 1.0 - inside;
  col += rimCol * exp(-o * mix(22.0, 6.0, E)) * outside * (0.05 + 0.62 * E + 0.10 * uHover) * (0.07 + lobes);
  col += mix(cA, cB, 0.5) * exp(-o * 2.6) * outside * 0.10 * E;

  // a hint of glass, so the sleeping orb is a dark ball and not a hole:
  // a fresnel edge and a soft window of light high on the left
  float fres = pow(1.0 - z, 2.6);
  float spec = pow(max(dot(vec3(p, z), vec3(-0.42, 0.62, 0.66)), 0.0), 9.0);
  vec3 glassCol = mix(vec3(0.62, 0.66, 1.0), vec3(1.0, 0.72, 0.55), uWarm);
  col += glassCol * inside * (fres * (0.085 + 0.05 * uHover) + spec * 0.035) * (1.0 - 0.7 * E);

  // click ripple: a ring that leaves the rim and fades
  float rp = uRipple;
  float rr = (r - 1.0 - 0.42 * (1.0 - (1.0 - rp) * (1.0 - rp))) / (0.012 + 0.05 * rp);
  col += mix(rimCol, vec3(1.0), 0.4) * exp(-rr * rr) * (1.0 - rp) * (1.0 - rp) * step(0.0005, rp) * 0.4;

  // nothing may touch the canvas edge
  col *= 1.0 - smoothstep(0.80, 0.99, length(uv));

  col = 1.0 - exp(-col * 1.35);
  float lum = max(col.r, max(col.g, col.b));
  col += (hash(gl_FragCoord.xy + fract(t) * 61.0) - 0.5) * (0.032 * lum + 1.5 / 255.0);  // film grain, kills banding
  col = max(col, 0.0);
  // premultiplied alpha: the ball's body hides the dot grid behind it
  float alpha = clamp(max(max(col.r, max(col.g, col.b)), inside * 0.94), 0.0, 1.0);
  vec4 dark = vec4(col, alpha);

  // Light mode: glow vanishes on paper, so the light turns into dye instead.
  // Strong light keeps its hue at full strength, darkness becomes frosted glass,
  // and the halo is a soft coloured haze rather than added light.
  float L = max(col.r, max(col.g, col.b));
  vec3 hue = col / max(L, 1e-3);
  vec3 frost = vec3(0.955, 0.950, 0.975);
  float dyeAmt = smoothstep(0.02, 0.85, L);
  vec3 lc = mix(frost, hue * (0.55 + 0.45 * L), dyeAmt * 0.92);
  lc *= 1.0 - 0.10 * fres * inside;                    // a shaded edge gives it volume
  lc = mix(lc, vec3(1.0), spec * 0.5 * inside);        // a soft window reflection
  float la = clamp(max(inside, smoothstep(0.0, 0.75, L) * 0.75) * (1.0 - smoothstep(0.80, 0.99, length(uv))), 0.0, 1.0);
  vec4 light = vec4(lc * la, la);

  outColor = mix(dark, light, uLight);
}`;

export interface OrbTarget {
  energy: number;
  spin: number;
  warm: number;
}

/** Sphere radius as a fraction of half the canvas size; the rest is room for the halo. */
export const ORB_RADIUS = 0.61;

// Every time coefficient in the shader is a multiple of 0.0001, so the clock can
// wrap here without a visible seam; it keeps float precision on long uptimes.
const TIME_WRAP = (2 * Math.PI) / 0.0001;

const UNIFORMS = ["uRes", "uTime", "uEnergy", "uSpin", "uWarm", "uHover", "uPress", "uRipple", "uRadius", "uLight"] as const;

export class Orb {
  private gl: WebGL2RenderingContext;
  private u = {} as Record<(typeof UNIFORMS)[number], WebGLUniformLocation | null>;
  private cur = { energy: 0, spin: 0, warm: 0, hover: 0, press: 0, ripple: 1, light: 0 };
  private target = { energy: 0, spin: 0, warm: 0, hover: 0, press: 0, light: 0 };
  private raf = 0;
  private last = 0;
  private time: number;
  private still = window.matchMedia("(prefers-reduced-motion: reduce)");
  private focused = document.hasFocus();

  static create(canvas: HTMLCanvasElement, time?: number): Orb | null {
    const gl = canvas.getContext("webgl2", {
      premultipliedAlpha: true,
      antialias: false,
      alpha: true,
      depth: false,
      stencil: false,
      powerPreference: "low-power",
    });
    if (!gl) return null;
    try {
      return new Orb(canvas, gl, time);
    } catch (e) {
      console.error(e);
      return null;
    }
  }

  private constructor(private canvas: HTMLCanvasElement, gl: WebGL2RenderingContext, time?: number) {
    this.gl = gl;
    // start somewhere different every launch; `time` pins it for screenshots
    this.time = time ?? (Date.now() % 600_000) / 1000;
    this.init();

    this.resize();
    new ResizeObserver(() => {
      this.resize();
      this.play();
    }).observe(canvas);
    document.addEventListener("visibilitychange", () => (document.hidden ? this.pause() : this.play()));
    window.addEventListener("blur", () => (this.focused = false));
    window.addEventListener("focus", () => {
      this.focused = true;
      this.play();
    });
    this.still.addEventListener?.("change", () => this.play());
    canvas.addEventListener("webglcontextlost", (e) => {
      e.preventDefault();
      this.pause();
    });
    canvas.addEventListener("webglcontextrestored", () => {
      this.init();
      this.resize();
      this.play();
    });
    this.play();
  }

  private init() {
    const gl = this.gl;
    const prog = gl.createProgram()!;
    for (const [type, src] of [[gl.VERTEX_SHADER, VERT], [gl.FRAGMENT_SHADER, FRAG]] as const) {
      const s = gl.createShader(type)!;
      gl.shaderSource(s, src);
      gl.compileShader(s);
      if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(s) ?? "shader");
      gl.attachShader(prog, s);
    }
    gl.linkProgram(prog);
    if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(prog) ?? "link");
    gl.useProgram(prog);
    const buf = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, buf);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 3, -1, -1, 3]), gl.STATIC_DRAW);
    const loc = gl.getAttribLocation(prog, "p");
    gl.enableVertexAttribArray(loc);
    gl.vertexAttribPointer(loc, 2, gl.FLOAT, false, 0, 0);
    for (const n of UNIFORMS) this.u[n] = gl.getUniformLocation(prog, n);
    gl.uniform1f(this.u.uRadius, ORB_RADIUS);
    gl.clearColor(0, 0, 0, 0);
  }

  /** Jump straight to a state (first paint), instead of easing into it. */
  snap(t: OrbTarget) {
    Object.assign(this.target, t);
    Object.assign(this.cur, t);
    this.play();
  }

  set(t: OrbTarget) {
    Object.assign(this.target, t);
    this.play();
  }

  /** Light or dark appearance; `instant` skips the cross-fade (first paint). */
  theme(light: boolean, instant = false) {
    this.target.light = light ? 1 : 0;
    if (instant) this.cur.light = this.target.light;
    this.play();
  }

  hover(on: boolean) {
    this.target.hover = on ? 1 : 0;
    this.play();
  }

  press(on: boolean) {
    this.target.press = on ? 1 : 0;
    this.play();
  }

  ripple() {
    this.cur.ripple = 0;
    this.play();
  }

  private resize() {
    const dpr = Math.min(window.devicePixelRatio || 1, 2);
    const w = Math.round(this.canvas.clientWidth * dpr);
    const h = Math.round(this.canvas.clientHeight * dpr);
    if (!w || !h) return;
    if (this.canvas.width !== w || this.canvas.height !== h) {
      this.canvas.width = w;
      this.canvas.height = h;
    }
    this.gl.viewport(0, 0, w, h);
    this.gl.uniform2f(this.u.uRes, w, h);
  }

  private play() {
    if (!this.raf && !document.hidden) {
      this.last = performance.now();
      this.raf = requestAnimationFrame(this.frame);
    }
  }

  private pause() {
    cancelAnimationFrame(this.raf);
    this.raf = 0;
  }

  private frame = (now: number) => {
    this.raf = requestAnimationFrame(this.frame);
    const c = this.cur, tg = this.target;
    // rAF timestamps can precede performance.now(): never let dt go negative
    const elapsed = Math.max((now - this.last) / 1000, 0);
    const settled =
      Math.abs(c.energy - tg.energy) < 0.004 &&
      Math.abs(c.spin - tg.spin) < 0.004 &&
      Math.abs(c.warm - tg.warm) < 0.004 &&
      Math.abs(c.hover - tg.hover) < 0.004 &&
      Math.abs(c.press - tg.press) < 0.004 &&
      Math.abs(c.light - tg.light) < 0.004 &&
      c.ripple >= 1;
    const asleep = tg.energy < 0.35;
    // Nothing is easing: the sleeping ring only drifts, 30 fps is plenty; and
    // a window in the background never needs more than that either. Awake, the
    // swirl gets 60 fps even where the display would offer 120.
    if (elapsed < (settled && (asleep || !this.focused) ? 1 / 31 : 1 / 75)) return;
    this.last = now;
    this.step(Math.min(elapsed, 0.1));
    this.draw();

    // Fully at rest (reduced motion, or asleep in a background window): stop
    // the loop altogether; set()/hover()/focus restart it.
    if (settled && (this.still.matches || (asleep && !this.focused))) this.pause();
  };

  private step(dt: number) {
    const c = this.cur, tg = this.target;
    const k = 1 - Math.exp(-dt * 0.9);   // slow, calm transitions
    c.energy += (tg.energy - c.energy) * k;
    c.spin += (tg.spin - c.spin) * k;
    c.warm += (tg.warm - c.warm) * k;
    c.hover += (tg.hover - c.hover) * (1 - Math.exp(-dt * 9));
    c.press += (tg.press - c.press) * (1 - Math.exp(-dt * 22));
    c.light += (tg.light - c.light) * (1 - Math.exp(-dt * 6));
    c.ripple = Math.min(1, c.ripple + dt / 1.6);
    // time runs faster while energised, so the swirl "wakes up"
    if (!this.still.matches) this.time += dt * 0.5 * (0.45 + 0.55 * c.energy + 0.25 * c.spin);
    if (this.time > TIME_WRAP) this.time -= TIME_WRAP;
  }

  private draw() {
    const gl = this.gl, u = this.u, c = this.cur;
    gl.uniform1f(u.uTime, this.time);
    gl.uniform1f(u.uEnergy, c.energy);
    gl.uniform1f(u.uSpin, c.spin);
    gl.uniform1f(u.uWarm, c.warm);
    gl.uniform1f(u.uHover, c.hover);
    gl.uniform1f(u.uPress, c.press);
    gl.uniform1f(u.uLight, c.light);
    gl.uniform1f(u.uRipple, this.still.matches ? 1 : c.ripple);
    gl.clear(gl.COLOR_BUFFER_BIT);
    gl.drawArrays(gl.TRIANGLES, 0, 3);
  }

  /** Dev: run the easing forward by `sec` at once, to screenshot a moment of a transition. */
  advance(sec: number) {
    for (let t = 0; t < sec; t += 1 / 60) this.step(1 / 60);
    this.draw();
    this.pause();
    this.raf = -1; // stay on this frame
  }
}
