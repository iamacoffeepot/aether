const canvas = document.querySelector("#viewport");
const status = document.querySelector("#status");
const controls = document.querySelector("#morph-controls");
const gl = canvas.getContext("webgl2", { antialias: true, alpha: true });

if (!gl) {
  fail("WebGL 2 is required for this demo");
  throw new Error("WebGL 2 unavailable");
}

const vertexSource = `#version 300 es
precision highp float;
in vec3 aPosition;
in vec3 aNormal;
in vec3 aTarget0;
in vec3 aTarget1;
in vec3 aTarget2;
in vec3 aTarget3;
in vec3 aTarget4;
in vec3 aTarget5;
in vec3 aTarget6;
in vec3 aTarget7;
in vec3 aTarget8;
in vec3 aTarget9;
uniform mat4 uViewProjection;
uniform mat4 uModel;
uniform mat3 uNormal;
uniform float uWeights[10];
out vec3 vNormal;
out vec3 vWorldPosition;
void main() {
  vec3 position = aPosition
    + aTarget0 * uWeights[0]
    + aTarget1 * uWeights[1]
    + aTarget2 * uWeights[2]
    + aTarget3 * uWeights[3]
    + aTarget4 * uWeights[4]
    + aTarget5 * uWeights[5]
    + aTarget6 * uWeights[6]
    + aTarget7 * uWeights[7]
    + aTarget8 * uWeights[8]
    + aTarget9 * uWeights[9];
  vec4 world = uModel * vec4(position, 1.0);
  vWorldPosition = world.xyz;
  vNormal = normalize(uNormal * aNormal);
  gl_Position = uViewProjection * world;
}`;

const fragmentSource = `#version 300 es
precision highp float;
in vec3 vNormal;
in vec3 vWorldPosition;
uniform vec4 uColor;
uniform vec3 uCamera;
out vec4 color;
void main() {
  vec3 normal = normalize(vNormal);
  vec3 light = normalize(vec3(-0.45, 0.65, 0.72));
  vec3 fill = normalize(vec3(0.55, 0.15, 0.42));
  float diffuse = max(dot(normal, light), 0.0);
  float fillLight = max(dot(normal, fill), 0.0) * 0.22;
  vec3 viewDirection = normalize(uCamera - vWorldPosition);
  float rim = pow(1.0 - max(dot(normal, viewDirection), 0.0), 2.8) * 0.18;
  float lighting = 0.24 + diffuse * 0.73 + fillLight + rim;
  color = vec4(uColor.rgb * lighting, uColor.a);
}`;

const program = createProgram(vertexSource, fragmentSource);
const locations = {
  position: gl.getAttribLocation(program, "aPosition"),
  normal: gl.getAttribLocation(program, "aNormal"),
  targets: Array.from({ length: 10 }, (_, index) => gl.getAttribLocation(program, `aTarget${index}`)),
  viewProjection: gl.getUniformLocation(program, "uViewProjection"),
  model: gl.getUniformLocation(program, "uModel"),
  normalMatrix: gl.getUniformLocation(program, "uNormal"),
  weights: gl.getUniformLocation(program, "uWeights"),
  color: gl.getUniformLocation(program, "uColor"),
  camera: gl.getUniformLocation(program, "uCamera"),
};

let scene;
let morphNames = [];
let weights = new Float32Array(10);
let yaw = 0;
let pitch = 0.02;
let distance = 3.45;
let dragging = false;
let lastPointer = [0, 0];

initialize().catch((error) => fail(error.message));

async function initialize() {
  const response = await fetch("/assets/aether-head.glb");
  if (!response.ok) throw new Error(`Head asset failed to load (${response.status})`);
  scene = createScene(await response.arrayBuffer());
  morphNames = scene.morphNames;
  createControls();
  bindInterface();
  status.classList.add("ready");
  status.innerHTML = `<span class="status-light"></span>${scene.triangleCount.toLocaleString()} triangles · ${morphNames.length} controls`;
  requestAnimationFrame(render);
}

function createScene(arrayBuffer) {
  const data = new DataView(arrayBuffer);
  if (data.getUint32(0, true) !== 0x46546c67 || data.getUint32(4, true) !== 2) {
    throw new Error("Asset is not a GLB 2.0 document");
  }
  const jsonLength = data.getUint32(12, true);
  const document = JSON.parse(new TextDecoder().decode(new Uint8Array(arrayBuffer, 20, jsonLength)));
  const binaryHeader = 20 + jsonLength;
  const binaryOffset = binaryHeader + 8;
  const access = (index) => readAccessor(arrayBuffer, binaryOffset, document, index);
  const meshes = document.meshes.map((mesh) => ({
    name: mesh.name,
    primitives: mesh.primitives.map((primitive) => createPrimitive(primitive, access, document)),
    targetNames: mesh.extras?.targetNames ?? [],
  }));
  const nodes = document.nodes
    .filter((node) => node.mesh !== undefined)
    .map((node) => ({
      name: node.name,
      mesh: meshes[node.mesh],
      translation: node.translation ?? [0, 0, 0],
      scale: node.scale ?? [1, 1, 1],
    }));
  return {
    nodes,
    morphNames: meshes.flatMap((mesh) => mesh.targetNames).slice(0, 10),
    triangleCount: meshes.reduce(
      (total, mesh) => total + mesh.primitives.reduce((sum, primitive) => sum + primitive.count / 3, 0),
      0,
    ),
  };
}

function createPrimitive(primitive, access, document) {
  const positions = access(primitive.attributes.POSITION);
  const vao = gl.createVertexArray();
  gl.bindVertexArray(vao);
  bindAttribute(locations.position, positions, 3);
  bindAttribute(locations.normal, access(primitive.attributes.NORMAL), 3);
  const targets = primitive.targets ?? [];
  locations.targets.forEach((location, index) => {
    if (targets[index]) bindAttribute(location, access(targets[index].POSITION), 3);
    else gl.disableVertexAttribArray(location);
  });
  const indices = access(primitive.indices);
  const indexBuffer = gl.createBuffer();
  gl.bindBuffer(gl.ELEMENT_ARRAY_BUFFER, indexBuffer);
  gl.bufferData(gl.ELEMENT_ARRAY_BUFFER, indices.values, gl.STATIC_DRAW);
  gl.bindVertexArray(null);
  return {
    vao,
    count: indices.count,
    indexType: indices.componentType,
    color: document.materials?.[primitive.material]?.pbrMetallicRoughness?.baseColorFactor ?? [0.7, 0.7, 0.7, 1],
    targetCount: targets.length,
    vertexCount: positions.count,
  };
}

function readAccessor(arrayBuffer, binaryOffset, document, accessorIndex) {
  const accessor = document.accessors[accessorIndex];
  const view = document.bufferViews[accessor.bufferView];
  const offset = binaryOffset + (view.byteOffset ?? 0) + (accessor.byteOffset ?? 0);
  const components = { SCALAR: 1, VEC2: 2, VEC3: 3, VEC4: 4 }[accessor.type];
  const constructors = { 5123: Uint16Array, 5125: Uint32Array, 5126: Float32Array };
  const Constructor = constructors[accessor.componentType];
  if (!Constructor || !components) throw new Error(`Unsupported accessor ${accessor.componentType}/${accessor.type}`);
  return {
    values: new Constructor(arrayBuffer, offset, accessor.count * components),
    count: accessor.count,
    componentType: accessor.componentType,
  };
}

function bindAttribute(location, accessor, size) {
  const buffer = gl.createBuffer();
  gl.bindBuffer(gl.ARRAY_BUFFER, buffer);
  gl.bufferData(gl.ARRAY_BUFFER, accessor.values, gl.STATIC_DRAW);
  gl.enableVertexAttribArray(location);
  gl.vertexAttribPointer(location, size, gl.FLOAT, false, 0, 0);
}

function createControls() {
  const friendlyNames = {
    JawWidth: "Jaw width",
    JawLength: "Jaw length",
    CheekVolume: "Cheek volume",
    NoseWidth: "Nose width",
    NoseLength: "Nose length",
    EyeSize: "Eye size",
    BrowHeight: "Brow height",
    LipFullness: "Lip fullness",
    MouthSmile: "Mouth smile",
    ChinShape: "Chin shape",
  };
  morphNames.forEach((name, index) => {
    const row = document.createElement("div");
    row.className = "morph-control";
    row.innerHTML = `
      <label for="morph-${index}">${friendlyNames[name] ?? name}</label>
      <output class="morph-value" for="morph-${index}">0.00</output>
      <input id="morph-${index}" type="range" min="-1" max="1" value="0" step="0.01">
    `;
    const input = row.querySelector("input");
    input.addEventListener("input", () => {
      weights[index] = Number(input.value);
      updateControl(row, weights[index]);
    });
    controls.append(row);
  });
}

function bindInterface() {
  canvas.addEventListener("pointerdown", (event) => {
    dragging = true;
    lastPointer = [event.clientX, event.clientY];
    canvas.setPointerCapture(event.pointerId);
  });
  canvas.addEventListener("pointermove", (event) => {
    if (!dragging) return;
    yaw += (event.clientX - lastPointer[0]) * 0.008;
    pitch = clamp(pitch + (event.clientY - lastPointer[1]) * 0.006, -1.1, 1.1);
    lastPointer = [event.clientX, event.clientY];
    clearActiveView();
  });
  canvas.addEventListener("pointerup", () => { dragging = false; });
  canvas.addEventListener("wheel", (event) => {
    event.preventDefault();
    distance = clamp(distance * Math.exp(event.deltaY * 0.001), 2.25, 6.5);
  }, { passive: false });

  document.querySelectorAll("[data-view]").forEach((button) => {
    button.addEventListener("click", () => {
      const views = { front: 0, "three-quarter": -0.62, profile: -1.48 };
      yaw = views[button.dataset.view];
      pitch = 0.02;
      document.querySelectorAll("[data-view]").forEach((candidate) => candidate.classList.toggle("active", candidate === button));
    });
  });
  document.querySelector("#reset").addEventListener("click", () => setWeights(new Array(10).fill(0)));
  document.querySelector("#randomize").addEventListener("click", () => {
    setWeights(morphNames.map(() => (Math.random() * 1.3) - 0.65));
  });
  document.querySelector("#export").addEventListener("click", exportRecipe);
  document.querySelector("#import").addEventListener("change", importRecipe);
}

function setWeights(values) {
  const inputs = controls.querySelectorAll("input");
  values.slice(0, 10).forEach((value, index) => {
    weights[index] = clamp(Number(value) || 0, -1, 1);
    inputs[index].value = weights[index];
    updateControl(inputs[index].closest(".morph-control"), weights[index]);
  });
}

function updateControl(row, value) {
  row.querySelector("output").value = value.toFixed(2);
  const percent = (value + 1) * 50;
  const start = Math.min(50, percent);
  const end = Math.max(50, percent);
  row.querySelector("input").style.background = `linear-gradient(to right, #344248 0 ${start}%, #76cbc6 ${start}% ${end}%, #344248 ${end}% 100%)`;
}

function exportRecipe() {
  const recipe = {
    format: "aether-character-recipe-v1",
    asset: "aether-head.glb",
    morphs: Object.fromEntries(morphNames.map((name, index) => [name, Number(weights[index].toFixed(3))])),
  };
  const link = document.createElement("a");
  link.href = URL.createObjectURL(new Blob([JSON.stringify(recipe, null, 2)], { type: "application/json" }));
  link.download = "aether-character.json";
  link.click();
  URL.revokeObjectURL(link.href);
}

async function importRecipe(event) {
  const file = event.target.files[0];
  if (!file) return;
  try {
    const recipe = JSON.parse(await file.text());
    if (recipe.format !== "aether-character-recipe-v1" || !recipe.morphs) throw new Error("Not an Aether character recipe");
    setWeights(morphNames.map((name) => recipe.morphs[name] ?? 0));
  } catch (error) {
    fail(error.message);
  } finally {
    event.target.value = "";
  }
}

function render() {
  resizeCanvas();
  gl.viewport(0, 0, canvas.width, canvas.height);
  gl.clearColor(0, 0, 0, 0);
  gl.clear(gl.COLOR_BUFFER_BIT | gl.DEPTH_BUFFER_BIT);
  gl.enable(gl.DEPTH_TEST);
  gl.useProgram(program);

  const camera = [
    Math.sin(yaw) * Math.cos(pitch) * distance,
    Math.sin(pitch) * distance,
    Math.cos(yaw) * Math.cos(pitch) * distance,
  ];
  const projection = perspective(Math.PI / 4.1, canvas.width / canvas.height, 0.05, 30);
  const view = lookAt(camera, [0, 0, 0.08], [0, 1, 0]);
  gl.uniformMatrix4fv(locations.viewProjection, false, multiply(projection, view));
  gl.uniform3fv(locations.camera, camera);
  gl.uniform1fv(locations.weights, weights);

  for (const node of scene.nodes) {
    const translation = [...node.translation];
    const scale = [...node.scale];
    const eyeSize = weights[morphNames.indexOf("EyeSize")] ?? 0;
    const browHeight = weights[morphNames.indexOf("BrowHeight")] ?? 0;
    if (node.name.startsWith("Eye.") || node.name.startsWith("Iris.") || node.name.startsWith("Pupil.")) {
      scale[0] *= 1 + eyeSize * 0.16;
      scale[1] *= 1 + eyeSize * 0.12;
      translation[2] -= Math.max(eyeSize, 0) * 0.040;
    }
    if (node.name === "UpperLids") {
      translation[1] += eyeSize * 0.006;
      translation[2] -= Math.max(eyeSize, 0) * 0.040;
    }
    if (node.name === "Brows") {
      translation[1] += browHeight * 0.10;
      translation[2] += browHeight * 0.025;
    }

    const model = modelMatrix(translation, scale);
    gl.uniformMatrix4fv(locations.model, false, model);
    gl.uniformMatrix3fv(locations.normalMatrix, false, normalMatrix(scale));
    for (const primitive of node.mesh.primitives) {
      gl.bindVertexArray(primitive.vao);
      locations.targets.slice(primitive.targetCount).forEach((location) => {
        gl.disableVertexAttribArray(location);
        gl.vertexAttrib3f(location, 0, 0, 0);
      });
      gl.uniform4fv(locations.color, primitive.color);
      gl.drawElements(gl.TRIANGLES, primitive.count, primitive.indexType, 0);
    }
  }
  gl.bindVertexArray(null);
  requestAnimationFrame(render);
}

function resizeCanvas() {
  const scale = Math.min(window.devicePixelRatio || 1, 2);
  const width = Math.floor(canvas.clientWidth * scale);
  const height = Math.floor(canvas.clientHeight * scale);
  if (canvas.width !== width || canvas.height !== height) {
    canvas.width = width;
    canvas.height = height;
  }
}

function createProgram(vertex, fragment) {
  const compile = (type, source) => {
    const shader = gl.createShader(type);
    gl.shaderSource(shader, source);
    gl.compileShader(shader);
    if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(shader));
    return shader;
  };
  const result = gl.createProgram();
  gl.attachShader(result, compile(gl.VERTEX_SHADER, vertex));
  gl.attachShader(result, compile(gl.FRAGMENT_SHADER, fragment));
  gl.linkProgram(result);
  if (!gl.getProgramParameter(result, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(result));
  return result;
}

function perspective(fieldOfView, aspect, near, far) {
  const f = 1 / Math.tan(fieldOfView / 2);
  const range = 1 / (near - far);
  return new Float32Array([
    f / aspect, 0, 0, 0,
    0, f, 0, 0,
    0, 0, (far + near) * range, -1,
    0, 0, 2 * far * near * range, 0,
  ]);
}

function lookAt(eye, target, up) {
  const z = normalize(subtract(eye, target));
  const x = normalize(cross(up, z));
  const y = cross(z, x);
  return new Float32Array([
    x[0], y[0], z[0], 0,
    x[1], y[1], z[1], 0,
    x[2], y[2], z[2], 0,
    -dot(x, eye), -dot(y, eye), -dot(z, eye), 1,
  ]);
}

function multiply(a, b) {
  const result = new Float32Array(16);
  for (let column = 0; column < 4; column += 1) {
    for (let row = 0; row < 4; row += 1) {
      result[column * 4 + row] =
        a[row] * b[column * 4]
        + a[4 + row] * b[column * 4 + 1]
        + a[8 + row] * b[column * 4 + 2]
        + a[12 + row] * b[column * 4 + 3];
    }
  }
  return result;
}

function modelMatrix(translation, scale) {
  return new Float32Array([
    scale[0], 0, 0, 0,
    0, scale[1], 0, 0,
    0, 0, scale[2], 0,
    translation[0], translation[1], translation[2], 1,
  ]);
}

function normalMatrix(scale) {
  return new Float32Array([
    1 / scale[0], 0, 0,
    0, 1 / scale[1], 0,
    0, 0, 1 / scale[2],
  ]);
}

function subtract(a, b) { return [a[0] - b[0], a[1] - b[1], a[2] - b[2]]; }
function dot(a, b) { return a[0] * b[0] + a[1] * b[1] + a[2] * b[2]; }
function cross(a, b) {
  return [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
}
function normalize(value) {
  const length = Math.hypot(...value) || 1;
  return value.map((component) => component / length);
}
function clamp(value, minimum, maximum) { return Math.max(minimum, Math.min(maximum, value)); }
function clearActiveView() { document.querySelectorAll("[data-view]").forEach((button) => button.classList.remove("active")); }
function fail(message) {
  status.className = "status error";
  status.innerHTML = `<span class="status-light"></span>${message}`;
}
