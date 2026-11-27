/**
 * Canonical Antigravity Companion twin-arch aurora logo tree.
 * Mirrors `plugin/assets/logo.svg` 1:1 (enforced by `ui/tests/views.test.ts`).
 */

export type SvgNodeSpec = readonly [
  tag: string,
  attrs: Readonly<Record<string, string | number>>,
  children?: readonly SvgNodeSpec[],
];

type GradientStopSpec = readonly [offset: string, color: string, opacity?: number | string];

function stopSpecs(stops: readonly GradientStopSpec[]): SvgNodeSpec[] {
  return stops.map(([offset, color, opacity]) =>
    opacity === undefined
      ? ["stop", { offset, "stop-color": color }]
      : ["stop", { offset, "stop-color": color, "stop-opacity": opacity }],
  );
}

export const BRAND_LOGO_CHILDREN: readonly SvgNodeSpec[] = [
  [
    "defs",
    {},
    [
      [
        "clipPath",
        { id: "tile-clip" },
        [["rect", { x: 6, y: 6, width: 116, height: 116, rx: 28 }]],
      ],
      [
        "clipPath",
        { id: "primary-clip" },
        [
          [
            "path",
            {
              d: "M22.5 92.5 C28.5 84 33.8 67.5 38.8 49.5 C43.6 32.5 50.2 24 62.5 24 C74.5 24 82.8 33 87 52 L73 56 C69.5 41.5 64 36.2 57.2 37.5 C50.2 38.8 46.5 49.2 42.5 63.2 C38 78.8 33.5 92.2 28.2 98.2 C25.8 100.8 21.5 99.8 20.8 96.8 C20.4 95 21.2 94.3 22.5 92.5 Z",
            },
          ],
        ],
      ],
      [
        "clipPath",
        { id: "companion-clip" },
        [
          [
            "path",
            {
              d: "M41.5 73.5 C48.2 56 59.5 41.5 75.5 41.5 C88.8 41.5 97 51.5 101 68.8 C103.6 80 105.4 88.8 108 93.4 C109.6 96.2 107.6 99.8 103.6 100.2 C98.2 100.8 93.8 96.8 90 88.8 C85.4 79 81.6 66.8 76.2 59.5 C71.2 52.8 64.5 51.2 57.6 54.8 C50.8 58.4 45.8 65.5 41.5 73.5 Z",
            },
          ],
        ],
      ],
      [
        "radialGradient",
        { id: "bg-grad", cx: 56, cy: 48, r: 88, gradientUnits: "userSpaceOnUse" },
        stopSpecs([
          ["0%", "#14193D"],
          ["50%", "#090B20"],
          ["100%", "#030410"],
        ]),
      ],
      [
        "linearGradient",
        { id: "tile-rim", x1: 64, y1: 6, x2: 64, y2: 122, gradientUnits: "userSpaceOnUse" },
        stopSpecs([
          ["0%", "#FFFFFF", 0.16],
          ["30%", "#A5B4FC", 0.06],
          ["100%", "#FFFFFF", 0.02],
        ]),
      ],
      [
        "filter",
        { id: "blur-lg", x: "-40%", y: "-40%", width: "180%", height: "180%" },
        [["feGaussianBlur", { stdDeviation: 8.5 }]],
      ],
      [
        "filter",
        { id: "blur-md", x: "-30%", y: "-30%", width: "160%", height: "160%" },
        [["feGaussianBlur", { stdDeviation: 3.4 }]],
      ],
      [
        "filter",
        { id: "blur-sm", x: "-25%", y: "-25%", width: "150%", height: "150%" },
        [["feGaussianBlur", { stdDeviation: 1.8 }]],
      ],
      [
        "linearGradient",
        { id: "aurora-vault", x1: 46, y1: 30, x2: 78, y2: 54, gradientUnits: "userSpaceOnUse" },
        stopSpecs([
          ["0%", "#FDE68A"],
          ["30%", "#FBBF24"],
          ["65%", "#EA580C"],
          ["100%", "#7C1D3A"],
        ]),
      ],
      [
        "linearGradient",
        { id: "aurora-main", x1: 62, y1: 24, x2: 22, y2: 99, gradientUnits: "userSpaceOnUse" },
        stopSpecs([
          ["0%", "#FEF08A"],
          ["17%", "#FDBA74"],
          ["36%", "#BEF264"],
          ["53%", "#6EE7B7"],
          ["73%", "#38BDF8"],
          ["92%", "#2563EB"],
          ["100%", "#1D4ED8"],
        ]),
      ],
      [
        "radialGradient",
        { id: "aurora-coral", cx: 77, cy: 36, r: 25, gradientUnits: "userSpaceOnUse" },
        stopSpecs([
          ["0%", "#FF5947", 0.96],
          ["52%", "#FB923C", 0.54],
          ["100%", "#FB923C", 0],
        ]),
      ],
      [
        "linearGradient",
        { id: "aurora-bevel", x1: 60, y1: 30, x2: 26, y2: 98, gradientUnits: "userSpaceOnUse" },
        stopSpecs([
          ["0%", "#FDE047", 0.55],
          ["28%", "#F97316", 0.62],
          ["54%", "#10B981", 0.58],
          ["80%", "#1D4ED8", 0.78],
          ["100%", "#172554", 0.92],
        ]),
      ],
      [
        "radialGradient",
        { id: "foot-left-glass", cx: 24, cy: 96, r: 9.5, gradientUnits: "userSpaceOnUse" },
        stopSpecs([
          ["0%", "#93C5FD", 0.92],
          ["55%", "#3B82F6", 0.4],
          ["100%", "#1D4ED8", 0],
        ]),
      ],
      [
        "linearGradient",
        { id: "sapphire-under", x1: 62, y1: 53, x2: 58, y2: 99, gradientUnits: "userSpaceOnUse" },
        stopSpecs([
          ["0%", "#60A5FA"],
          ["28%", "#3B82F6"],
          ["68%", "#1D4ED8"],
          ["100%", "#172554"],
        ]),
      ],
      [
        "linearGradient",
        { id: "comp-under", x1: 56, y1: 51, x2: 94, y2: 96, gradientUnits: "userSpaceOnUse" },
        stopSpecs([
          ["0%", "#60A5FA", 0.88],
          ["44%", "#2563EB", 0.72],
          ["100%", "#0F172A", 0.9],
        ]),
      ],
      [
        "linearGradient",
        { id: "comp-base", x1: 42, y1: 62, x2: 105, y2: 92, gradientUnits: "userSpaceOnUse" },
        stopSpecs([
          ["0%", "#60A5FA"],
          ["14%", "#CBD5E1"],
          ["38%", "#F8FAFC"],
          ["66%", "#E2E8F0"],
          ["88%", "#CBD5E1"],
          ["100%", "#F1F5F9"],
        ]),
      ],
      [
        "linearGradient",
        { id: "comp-flank", x1: 76, y1: 43, x2: 107, y2: 98, gradientUnits: "userSpaceOnUse" },
        stopSpecs([
          ["0%", "#FFFFFF", 0.96],
          ["45%", "#F8FAFC", 0.92],
          ["82%", "#E2E8F0", 0.88],
          ["100%", "#FFFFFF", 0.95],
        ]),
      ],
      [
        "radialGradient",
        { id: "comp-irid-peach", cx: 65, cy: 52, r: 21, gradientUnits: "userSpaceOnUse" },
        stopSpecs([
          ["0%", "#FDE68A", 0.62],
          ["45%", "#FDA4AF", 0.46],
          ["100%", "#FDA4AF", 0],
        ]),
      ],
      [
        "radialGradient",
        { id: "comp-irid-opal", cx: 82, cy: 54, r: 22, gradientUnits: "userSpaceOnUse" },
        stopSpecs([
          ["0%", "#67E8F9", 0.54],
          ["50%", "#A7F3D0", "0.30"],
          ["100%", "#38BDF8", 0],
        ]),
      ],
      [
        "radialGradient",
        { id: "comp-irid-violet", cx: 74, cy: 65, r: 18, gradientUnits: "userSpaceOnUse" },
        stopSpecs([
          ["0%", "#C4B5FD", 0.38],
          ["100%", "#818CF8", 0],
        ]),
      ],
      [
        "linearGradient",
        { id: "comp-crease", x1: 56, y1: 54, x2: 94, y2: 95, gradientUnits: "userSpaceOnUse" },
        stopSpecs([
          ["0%", "#3B82F6", 0.35],
          ["45%", "#94A3B8", 0.42],
          ["100%", "#64748B", 0.32],
        ]),
      ],
      [
        "linearGradient",
        { id: "spec-primary", x1: 33, y1: 74, x2: 77, y2: 25, gradientUnits: "userSpaceOnUse" },
        stopSpecs([
          ["0%", "#6EE7B7", 0],
          ["38%", "#FEF3C7", 0.88],
          ["76%", "#FFFFFF", 0.94],
          ["100%", "#FF6B4A", 0],
        ]),
      ],
      [
        "linearGradient",
        { id: "spec-companion", x1: 48, y1: 60, x2: 106, y2: 95, gradientUnits: "userSpaceOnUse" },
        stopSpecs([
          ["0%", "#BAE6FD", 0],
          ["26%", "#FFFFFF", 0.96],
          ["68%", "#E0F2FE", 0.72],
          ["100%", "#FFFFFF", 0.38],
        ]),
      ],
      [
        "linearGradient",
        { id: "spec-sapphire", x1: 31, y1: 93, x2: 88, y2: 91, gradientUnits: "userSpaceOnUse" },
        stopSpecs([
          ["0%", "#60A5FA", 0],
          ["46%", "#BAE6FD", 0.95],
          ["100%", "#60A5FA", 0],
        ]),
      ],
    ],
  ],
  ["rect", { x: 6, y: 6, width: 116, height: 116, rx: 28, fill: "url(#bg-grad)" }],
  [
    "g",
    { "clip-path": "url(#tile-clip)" },
    [
      [
        "g",
        { filter: "url(#blur-lg)" },
        [
          ["ellipse", { cx: 61, cy: 35, rx: 25, ry: 16, fill: "#FF6B4A", "fill-opacity": "0.35" }],
          ["ellipse", { cx: 36, cy: 66, rx: 16, ry: 22, fill: "#34D399", "fill-opacity": "0.24" }],
          ["ellipse", { cx: 46, cy: 89, rx: 28, ry: 17, fill: "#2563EB", "fill-opacity": "0.40" }],
          ["ellipse", { cx: 87, cy: 70, rx: 20, ry: 22, fill: "#BAE6FD", "fill-opacity": "0.14" }],
        ],
      ],
      [
        "path",
        {
          d: "M44 48 C50 32 66 28 81 47 L67 55 C60 44 53 43 46 52 Z",
          fill: "url(#aurora-vault)",
        },
      ],
      [
        "g",
        { "clip-path": "url(#primary-clip)" },
        [
          ["rect", { x: 18, y: 21, width: 74, height: 82, fill: "url(#aurora-main)" }],
          ["rect", { x: 18, y: 21, width: 74, height: 82, fill: "url(#aurora-coral)" }],
          [
            "path",
            {
              d: "M58 35 C65.5 34 71 39 75.5 52 L65 56 C61 46.5 55.5 44 51 46 C46 48.5 43 58 39.2 70 C35.5 82 31.5 92.5 26.8 99.5 L24 99.5 C30 91 36 76.5 41 60.5 C45.5 46 49.5 36.5 58 35 Z",
              fill: "url(#aurora-bevel)",
              filter: "url(#blur-sm)",
            },
          ],
          ["circle", { cx: 24, cy: 96, r: 9.5, fill: "url(#foot-left-glass)" }],
        ],
      ],
      [
        "path",
        {
          d: "M26 97.6 C29.2 98.4 32.4 97 35.4 93.4 C41.4 85.8 46 73.2 52.2 65.2 C57.6 58.2 64.8 57.6 70.8 62.8 C77 68.2 82.4 78.6 88.2 89 C92 95.6 96.6 99.2 101.2 99.2 C96.4 97 92.4 90.6 87.4 80.2 C81.6 68.5 76 57.8 68.6 53.6 C60.4 48.8 51.8 52.8 45.6 63.5 C39.4 74.2 35.2 86.8 29 94.2 C27.6 95.8 26.6 97 26 97.6 Z",
          fill: "url(#sapphire-under)",
        },
      ],
      [
        "path",
        {
          d: "M43 70 C50.5 52 63.5 41.5 78.5 44 C84.5 45 89 49.5 92 56.5",
          stroke: "#040615",
          "stroke-width": 7.5,
          "stroke-linecap": "round",
          "stroke-opacity": 0.64,
          filter: "url(#blur-md)",
        },
      ],
      [
        "path",
        {
          d: "M41.8 73 C47.8 58 57.5 48.2 67.8 48.2 C77.5 48.2 84.5 55.2 89 68.8 C93 80.8 96 92.5 102.2 99.2 C95.6 98.6 90.6 92.5 85.8 81.6 C80.8 70.4 75.2 60.4 66.8 58.2 C58 56 50 62.5 41.8 73 Z",
          fill: "url(#comp-under)",
        },
      ],
      [
        "g",
        { "clip-path": "url(#companion-clip)" },
        [
          ["rect", { x: 40, y: 40, width: 72, height: 64, fill: "url(#comp-base)" }],
          ["rect", { x: 40, y: 40, width: 72, height: 64, fill: "url(#comp-irid-peach)" }],
          ["rect", { x: 40, y: 40, width: 72, height: 64, fill: "url(#comp-irid-opal)" }],
          ["rect", { x: 40, y: 40, width: 72, height: 64, fill: "url(#comp-irid-violet)" }],
          [
            "path",
            {
              d: "M68 42.5 C83 40.5 95 49.5 100.2 67.8 C103.4 79.8 105.6 89 108.5 94 C110 96.8 108 100.4 103.8 100.6 C100.4 100.8 97.8 99.2 95.8 96.2 C93.4 92.6 91.2 84 88.2 71.8 C84.8 57.8 79.5 47.2 68 42.5 Z",
              fill: "url(#comp-flank)",
              filter: "url(#blur-sm)",
            },
          ],
          [
            "path",
            {
              d: "M44 70 C50.5 56 60.2 48.2 70.5 49.5 C79.2 50.8 84.8 59.5 89.6 74.8 C92.8 85 95.5 93.8 99.8 98.8",
              stroke: "url(#comp-crease)",
              "stroke-width": 3.2,
              filter: "url(#blur-sm)",
            },
          ],
        ],
      ],
      [
        "path",
        {
          d: "M34.5 64 C39.8 44.5 46.8 26.2 61 24.8 C69 24 75.5 28.2 80 35.8",
          stroke: "url(#spec-primary)",
          "stroke-width": 1.6,
          "stroke-linecap": "round",
        },
      ],
      [
        "path",
        {
          d: "M52 57 C61.2 46 70.5 42.2 79.8 44.5 C90.2 47.2 96 59.2 100.2 76.5 C102.6 86.2 104.8 92.8 107 95.8",
          stroke: "url(#spec-companion)",
          "stroke-width": 1.6,
          "stroke-linecap": "round",
        },
      ],
      [
        "path",
        {
          d: "M33.5 93 C40.8 81.2 46.8 67.5 54 62.6 C60.4 58.2 67 59 72.4 64.2 C78.4 70.4 83.4 82.2 89.4 91.5",
          stroke: "url(#spec-sapphire)",
          "stroke-width": 1.35,
          "stroke-linecap": "round",
        },
      ],
    ],
  ],
  [
    "rect",
    {
      x: 6.5,
      y: 6.5,
      width: 115,
      height: 115,
      rx: 27.5,
      stroke: "url(#tile-rim)",
      "stroke-width": 1,
    },
  ],
];
