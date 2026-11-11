#!/usr/bin/env node
/**
 * Склейка капчи для Azalea-бота: stdin JSON → stdout { pngBase64 }.
 * Стену: pickCaptchaWallFrames + геометрический fallback (Azalea facing часто врёт).
 *
 * stdin: {
 *   frames: [...],
 *   maps: { "id": "base64..." },
 *   origin?: { x, y, z },
 *   botYaw?: number,
 *   botPitch?: number,
 * }
 */
import { createRequire } from 'module';
import { writeFileSync } from 'fs';
import { assembleCaptcha } from './captcha/assemble.mjs';
import {
    inspectCaptchaPng,
    pickCaptchaWallFrames,
} from './captcha/quality.mjs';

const require = createRequire(import.meta.url);
const EntityUtils = require('./vendor/flayercaptcha/utils/entityUtils.js');

const MIN_FRAMES = 12;

/** Сырой stdin последней неудачи (все рамки + все карты) — для разбора; один файл, перезаписывается. */
let RAW_INPUT = '';
function saveFailInput(tag) {
    try {
        writeFileSync(`/tmp/captcha-fail-input-${tag}.json`, RAW_INPUT);
    } catch {
        /* ignore */
    }
}

async function readStdin() {
    const chunks = [];
    for await (const c of process.stdin) chunks.push(c);
    return Buffer.concat(chunks).toString('utf8');
}

function diagFail(frames, origin) {
    const withMaps = frames.filter((f) => f && f.mapId != null && f._hasMap);
    const byDir = {};
    for (const f of withMaps) {
        const d = f.viewDirection || '?';
        byDir[d] = (byDir[d] || 0) + 1;
    }
    const uniqMaps = new Set(withMaps.map((f) => f.mapId)).size;
    const sample = withMaps.slice(0, 8).map((f) => ({
        mapId: f.mapId,
        view: f.viewDirection,
        facing: f.facing || null,
        x: +f.coordinate.x.toFixed(3),
        y: +f.coordinate.y.toFixed(3),
        z: +f.coordinate.z.toFixed(3),
        rot: f.rotate,
    }));
    const planes = {};
    for (const f of withMaps) {
        const d = f.viewDirection || 'north';
        const wallKey = d === 'east' || d === 'west' ? 'x' : d === 'up' || d === 'down' ? 'y' : 'z';
        const wk = Math.round(f.coordinate[wallKey]);
        const key = `${d}@${wallKey}=${wk}`;
        planes[key] = (planes[key] || 0) + 1;
    }
    return {
        frames: frames.length,
        withMaps: withMaps.length,
        uniqMaps,
        byDir,
        planes,
        origin,
        sample,
    };
}

/**
 * flayercaptcha viewDirection для стены с нормалью к игроку.
 * wallKey=z, player южнее стены (z < W) → рамки смотрят на север → view=south.
 */
function inferView(wallKey, wallVal, origin) {
    if (!origin) {
        if (wallKey === 'x') return 'west';
        if (wallKey === 'y') return 'down';
        return 'south';
    }
    const o = origin[wallKey];
    if (wallKey === 'z') {
        // facing north (к -z) → view south; facing south → view north
        return o < wallVal ? 'south' : 'north';
    }
    if (wallKey === 'x') {
        // facing west (к -x) → view west; facing east → view east
        return o < wallVal ? 'west' : 'east';
    }
    // y: facing up → view down; facing down → view up
    return o < wallVal ? 'down' : 'up';
}

function nearestRectOnPlane(plane, origin, widthKey, heightKey, minFrames) {
    const wantCols = 4;
    const wantRows = 3;
    if (wantCols * wantRows !== minFrames) return null;

    const colVals = [...new Set(plane.map((f) => Math.round(f.coordinate[widthKey])))].sort(
        (a, b) => a - b,
    );
    const rowVals = [...new Set(plane.map((f) => Math.round(f.coordinate[heightKey])))].sort(
        (a, b) => b - a,
    );
    if (colVals.length < wantCols || rowVals.length < wantRows) return null;

    const byCell = new Map();
    for (const f of plane) {
        byCell.set(
            `${Math.round(f.coordinate[widthKey])},${Math.round(f.coordinate[heightKey])}`,
            f,
        );
    }

    let best = null;
    let bestDist = Infinity;
    for (let ci = 0; ci <= colVals.length - wantCols; ci++) {
        for (let ri = 0; ri <= rowVals.length - wantRows; ri++) {
            const pick = [];
            let ok = true;
            for (let dc = 0; dc < wantCols && ok; dc++) {
                for (let dr = 0; dr < wantRows; dr++) {
                    const f = byCell.get(`${colVals[ci + dc]},${rowVals[ri + dr]}`);
                    if (!f) {
                        ok = false;
                        break;
                    }
                    pick.push(f);
                }
            }
            if (!ok || pick.length !== minFrames) continue;
            const dist = origin
                ? pick.reduce(
                      (s, f) =>
                          s +
                          Math.hypot(
                              f.coordinate.x - origin.x,
                              f.coordinate.y - origin.y,
                              f.coordinate.z - origin.z,
                          ),
                      0,
                  ) / pick.length
                : 0;
            if (dist < bestDist) {
                bestDist = dist;
                best = pick;
            }
        }
    }
    return best;
}

/** Геометрия: ищем 4×3 на оси X/Y/Z, viewDirection выводим от origin. */
function pickWallGeometric(frames, origin, minFrames = MIN_FRAMES) {
    const axes = [
        { wallKey: 'z', widthKey: 'x', heightKey: 'y' },
        { wallKey: 'x', widthKey: 'z', heightKey: 'y' },
        { wallKey: 'y', widthKey: 'x', heightKey: 'z' },
    ];

    /** @type {{ frames: typeof frames, score: number }[]} */
    const candidates = [];

    for (const { wallKey, widthKey, heightKey } of axes) {
        const planes = new Map();
        for (const f of frames) {
            const wk = Math.round(f.coordinate[wallKey]);
            if (!planes.has(wk)) planes.set(wk, []);
            planes.get(wk).push(f);
        }
        for (const [wallVal, plane] of planes) {
            if (plane.length < minFrames) continue;
            const mapIds = new Set(plane.map((f) => f.mapId));
            if (mapIds.size < minFrames) continue;

            const colVals = [...new Set(plane.map((f) => Math.round(f.coordinate[widthKey])))].sort(
                (a, b) => a - b,
            );
            const rowVals = [
                ...new Set(plane.map((f) => Math.round(f.coordinate[heightKey]))),
            ].sort((a, b) => b - a);
            const cells = new Set(
                plane.map(
                    (f) =>
                        `${Math.round(f.coordinate[widthKey])},${Math.round(f.coordinate[heightKey])}`,
                ),
            );
            if (cells.size !== plane.length) continue;
            let filled = 0;
            for (const c of colVals) {
                for (const r of rowVals) {
                    if (cells.has(`${c},${r}`)) filled += 1;
                }
            }
            if (filled !== colVals.length * rowVals.length) continue;

            let subset = plane;
            if (filled > minFrames) {
                subset = nearestRectOnPlane(plane, origin, widthKey, heightKey, minFrames);
                if (!subset || subset.length < minFrames) continue;
            } else {
                const cols = colVals.length;
                const rows = rowVals.length;
                const shapeOk =
                    (cols === 4 && rows === 3) ||
                    (cols === 3 && rows === 4) ||
                    filled === minFrames;
                if (!shapeOk) continue;
            }

            const view = inferView(wallKey, wallVal, origin);
            const tagged = subset.map((f) => ({ ...f, viewDirection: view }));
            const dist = origin
                ? tagged.reduce(
                      (s, f) =>
                          s +
                          Math.hypot(
                              f.coordinate.x - origin.x,
                              f.coordinate.y - origin.y,
                              f.coordinate.z - origin.z,
                          ),
                      0,
                  ) / tagged.length
                : 0;
            candidates.push({
                frames: tagged,
                score: 1000 + tagged.length * 10 - dist,
            });
        }
    }

    if (!candidates.length) return null;
    candidates.sort((a, b) => b.score - a.score);
    return candidates[0].frames;
}

/**
 * Взгляд бота (yaw/pitch в радианах, конвенция Minecraft: yaw 0 = +z/юг, 90° = −x/запад; pitch > 0 = вниз).
 * Rust шлёт LookDirection Azalea как есть (градусы → радианы), без mineflayer-сдвига.
 */
function lookVector(yaw, pitch) {
    return {
        x: -Math.sin(yaw) * Math.cos(pitch),
        y: -Math.sin(pitch),
        z: Math.cos(yaw) * Math.cos(pitch),
    };
}

/**
 * Все прямоугольники 4×3 / 3×4 из соседних рамок на одной плоскости (оси z/x/y).
 * BotFilter ставит ДВЕ стены: настоящую перед игроком и перемешанную приманку за спиной.
 * Раньше брали ближайшую — а ближе часто оказывалась приманка.
 */
function wallWindows(frames, minFrames = MIN_FRAMES) {
    const axes = [
        { wallKey: 'z', widthKey: 'x', heightKey: 'y' },
        { wallKey: 'x', widthKey: 'z', heightKey: 'y' },
        { wallKey: 'y', widthKey: 'x', heightKey: 'z' },
    ];
    const shapes = [
        [4, 3],
        [3, 4],
    ].filter(([w, h]) => w * h === minFrames);
    const out = [];
    const seen = new Set();
    for (const { wallKey, widthKey, heightKey } of axes) {
        const planes = new Map();
        for (const f of frames) {
            const wk = Math.round(f.coordinate[wallKey]);
            if (!planes.has(wk)) planes.set(wk, []);
            planes.get(wk).push(f);
        }
        for (const [wallVal, plane] of planes) {
            if (plane.length < minFrames) continue;
            const byCell = new Map();
            for (const f of plane) {
                const k = `${Math.round(f.coordinate[widthKey])},${Math.round(f.coordinate[heightKey])}`;
                if (!byCell.has(k)) byCell.set(k, []);
                byCell.get(k).push(f);
            }
            const colVals = [...new Set(plane.map((f) => Math.round(f.coordinate[widthKey])))].sort(
                (a, b) => a - b,
            );
            const rowVals = [
                ...new Set(plane.map((f) => Math.round(f.coordinate[heightKey]))),
            ].sort((a, b) => b - a);
            for (const [w, h] of shapes) {
                for (let ci = 0; ci + w <= colVals.length; ci++) {
                    // только соседние клетки: две стены через промежуток не склеиваем
                    if (colVals[ci + w - 1] - colVals[ci] !== w - 1) continue;
                    for (let ri = 0; ri + h <= rowVals.length; ri++) {
                        if (rowVals[ri] - rowVals[ri + h - 1] !== h - 1) continue;
                        const pick = [];
                        let ok = true;
                        for (let dc = 0; dc < w && ok; dc++) {
                            for (let dr = 0; dr < h; dr++) {
                                const cell = byCell.get(`${colVals[ci + dc]},${rowVals[ri + dr]}`);
                                // пусто или две рамки в одной клетке — окно не годится
                                if (!cell || cell.length !== 1) {
                                    ok = false;
                                    break;
                                }
                                pick.push(cell[0]);
                            }
                        }
                        if (!ok || pick.length !== minFrames) continue;
                        if (new Set(pick.map((f) => f.mapId)).size !== minFrames) continue;
                        const key = pick
                            .map((f) => f.mapId)
                            .sort((a, b) => a - b)
                            .join(',');
                        if (seen.has(key)) continue;
                        seen.add(key);
                        out.push({ frames: pick, wallKey, wallVal });
                    }
                }
            }
        }
    }
    return out;
}

/**
 * Кандидаты стен по порядку: сначала те, что прямо перед ботом (по его взгляду), потом ближние.
 * facing рамок у Azalea часто врёт (у всех «down»), поэтому направление считаем из геометрии.
 * В конце — старые пикеры как запасной вариант.
 */
function wallCandidates(frames, origin, botYaw, botPitch) {
    const withMaps = frames.filter((f) => f && f.mapId != null && f._hasMap);
    if (withMaps.length < MIN_FRAMES) return [];

    const look =
        Number.isFinite(botYaw) && Number.isFinite(botPitch) ? lookVector(botYaw, botPitch) : null;
    const ranked = wallWindows(withMaps).map((c) => {
        const n = c.frames.length;
        const center = {
            x: c.frames.reduce((s, f) => s + f.coordinate.x, 0) / n,
            y: c.frames.reduce((s, f) => s + f.coordinate.y, 0) / n,
            z: c.frames.reduce((s, f) => s + f.coordinate.z, 0) / n,
        };
        let dist = 0;
        let dot = 0;
        if (origin) {
            const v = { x: center.x - origin.x, y: center.y - origin.y, z: center.z - origin.z };
            dist = Math.hypot(v.x, v.y, v.z) || 1e-6;
            if (look) dot = (v.x * look.x + v.y * look.y + v.z * look.z) / dist;
        }
        const view = inferView(c.wallKey, c.wallVal, origin);
        return {
            frames: c.frames.map((f) => ({ ...f, viewDirection: view })),
            dot,
            dist,
            source: 'window',
        };
    });
    ranked.sort((a, b) => {
        const fa = a.dot > 0.5 ? 0 : 1;
        const fb = b.dot > 0.5 ? 0 : 1;
        if (fa !== fb) return fa - fb;
        if (Math.abs(a.dot - b.dot) > 0.05) return b.dot - a.dot;
        return a.dist - b.dist;
    });

    const out = [...ranked];
    const seen = new Set(out.map((c) => c.frames.map((f) => f.mapId).sort((a, b) => a - b).join(',')));
    const legacy = pickWall(frames, origin, botYaw, botPitch);
    if (legacy?.length >= MIN_FRAMES) {
        const key = legacy
            .map((f) => f.mapId)
            .sort((a, b) => a - b)
            .join(',');
        if (!seen.has(key)) out.push({ frames: legacy, dot: 0, dist: 0, source: 'legacy' });
    }
    return out;
}

/** Как capture.mjs pickFrames + geometric fallback. */
function pickWall(frames, origin, botYaw, botPitch) {
    const withMaps = frames.filter((f) => f && f.mapId != null && f._hasMap);
    if (withMaps.length < MIN_FRAMES) return null;

    let pool = withMaps;
    if (Number.isFinite(botYaw) && Number.isFinite(botPitch)) {
        const botEnt = { yaw: botYaw, pitch: botPitch };
        const fwd = withMaps.filter(
            (f) => EntityUtils.getFacing(botEnt, f.viewDirection) === 'forward',
        );
        if (fwd.length >= MIN_FRAMES) pool = fwd;
    }

    let wall = pickCaptchaWallFrames(pool, origin, MIN_FRAMES);
    if (wall?.length >= MIN_FRAMES) return wall;

    const byDir = new Map();
    for (const f of pool) {
        const d = f.viewDirection || 'south';
        if (!byDir.has(d)) byDir.set(d, []);
        byDir.get(d).push(f);
    }
    let best = [];
    for (const list of byDir.values()) {
        if (list.length > best.length) best = list;
    }
    if (best.length < MIN_FRAMES) best = pool;
    wall = pickCaptchaWallFrames(best, origin, MIN_FRAMES);
    if (wall?.length >= MIN_FRAMES) return wall;

    // Azalea: facing/look часто «down» на вертикальной стене — берём геометрию
    return pickWallGeometric(withMaps, origin, MIN_FRAMES);
}

function main() {
    readStdin()
        .then(async (raw) => {
            RAW_INPUT = raw;
            const msg = JSON.parse(raw || '{}');
            const framesIn = Array.isArray(msg.frames) ? msg.frames : [];
            const mapsObj = msg.maps && typeof msg.maps === 'object' ? msg.maps : {};
            /** @type {Map<number, Buffer>} */
            const mapsRaw = new Map();
            for (const [k, v] of Object.entries(mapsObj)) {
                const id = Number(k);
                if (!Number.isFinite(id) || typeof v !== 'string') continue;
                mapsRaw.set(id, Buffer.from(v, 'base64'));
            }

            const frames = framesIn.map((f) => {
                const mapId = Number(f.mapId);
                return {
                    mapId,
                    rotate: Number(f.rotate ?? 0) & 7,
                    coordinate: {
                        x: Number(f.coordinate?.x ?? f.x ?? 0),
                        y: Number(f.coordinate?.y ?? f.y ?? 0),
                        z: Number(f.coordinate?.z ?? f.z ?? 0),
                    },
                    viewDirection: String(f.viewDirection || f.view || 'south'),
                    facing: f.facing != null ? String(f.facing) : null,
                    _hasMap: mapsRaw.has(mapId),
                };
            });

            const origin =
                msg.origin && typeof msg.origin === 'object'
                    ? {
                          x: Number(msg.origin.x),
                          y: Number(msg.origin.y),
                          z: Number(msg.origin.z),
                      }
                    : null;
            const botYaw = Number(msg.botYaw);
            const botPitch = Number(msg.botPitch);

            const cands = wallCandidates(frames, origin, botYaw, botPitch);
            if (!cands.length) {
                const diag = diagFail(frames, origin);
                try {
                    writeFileSync(
                        '/tmp/captcha-wall-fail.json',
                        JSON.stringify(
                            {
                                ...diag,
                                botYaw: Number.isFinite(botYaw) ? botYaw : null,
                                botPitch: Number.isFinite(botPitch) ? botPitch : null,
                                all: frames
                                    .filter((f) => f._hasMap)
                                    .map((f) => ({
                                        mapId: f.mapId,
                                        view: f.viewDirection,
                                        facing: f.facing,
                                        rot: f.rotate,
                                        x: f.coordinate.x,
                                        y: f.coordinate.y,
                                        z: f.coordinate.z,
                                    })),
                            },
                            null,
                            2,
                        ),
                    );
                } catch {
                    /* ignore */
                }
                process.stdout.write(
                    JSON.stringify({
                        ok: false,
                        error: `wall pick fail frames=${diag.frames} withMaps=${diag.withMaps} byDir=${JSON.stringify(diag.byDir)} planes=${JSON.stringify(diag.planes)} uniqMaps=${diag.uniqMaps}`,
                        diag,
                    }),
                );
                process.exit(2);
            }

            // Перебор view × rotMode × rotOff — Azalea rot/facing часто не как mineflayer.
            const viewAlt = {
                south: 'north',
                north: 'south',
                east: 'west',
                west: 'east',
                up: 'down',
                down: 'up',
            };
            const rotModes = [
                { name: 'as_is', fn: (r) => r & 7 },
                { name: 'mod4', fn: (r) => r & 3 },
                { name: 'half', fn: (r) => (r >> 1) & 3 }, // 45° units → 90° turns
                { name: 'neg', fn: (r) => (-(r & 3) + 4) & 3 },
                { name: 'zero', fn: () => 0 },
            ];
            /** @type {{ png: Buffer, quality: ReturnType<typeof inspectCaptchaPng>, view: string, rotOff: number, rotMode: string, wall: typeof frames }[]} */
            const tries = [];
            // Стены по порядку (сначала перед ботом); первая, что склеилась без разрывов на швах, — ответ.
            for (let ci = 0; ci < cands.length; ci++) {
                const cand = cands[ci];
                const wall = cand.frames;
                const baseView = wall[0]?.viewDirection || 'south';
                for (const view of [baseView, viewAlt[baseView] || baseView]) {
                    for (const mode of rotModes) {
                        for (const rotOff of [0, 1, 2, 3]) {
                            const tagged = wall.map((f) => ({
                                ...f,
                                viewDirection: view,
                                rotate: (mode.fn(f.rotate | 0) + rotOff) & 3,
                            }));
                            const { png } = await assembleCaptcha(tagged, mapsRaw);
                            const quality = inspectCaptchaPng(png);
                            tries.push({ png, quality, view, rotOff, rotMode: mode.name, wall });
                            if (quality.ok) {
                                process.stdout.write(
                                    JSON.stringify({
                                        ok: true,
                                        pngBase64: png.toString('base64'),
                                        wallSize: wall.length,
                                        wallMapIds: wall.map((f) => f.mapId),
                                        wallView: view,
                                        wallRank: ci,
                                        wallCandidates: cands.length,
                                        wallDot: +cand.dot.toFixed(2),
                                        wallSource: cand.source,
                                        rotOff,
                                        rotMode: mode.name,
                                        quality: {
                                            cols: quality.cols,
                                            rows: quality.rows,
                                            seamMean: quality.seamMean,
                                            blackRatio: quality.blackRatio,
                                        },
                                    }),
                                );
                                return;
                            }
                        }
                    }
                }
            }
            tries.sort((a, b) => a.quality.seamMean - b.quality.seamMean);
            const best = tries[0];
            const wall = best.wall;
            const quality = best.quality;
            const png = best.png;
            if (!quality.ok) {
                saveFailInput('quality');
                try {
                    writeFileSync('/tmp/captcha-quality-fail.png', png);
                    const mapsDump = {};
                    for (const f of wall) {
                        const buf = mapsRaw.get(f.mapId);
                        if (buf) mapsDump[String(f.mapId)] = buf.toString('base64');
                    }
                    writeFileSync(
                        '/tmp/captcha-quality-fail.json',
                        JSON.stringify(
                            {
                                quality,
                                bestTry: {
                                    view: best.view,
                                    rotOff: best.rotOff,
                                    rotMode: best.rotMode,
                                    seamMean: quality.seamMean,
                                },
                                tries: tries.slice(0, 12).map((t) => ({
                                    view: t.view,
                                    rotOff: t.rotOff,
                                    rotMode: t.rotMode,
                                    seamMean: t.quality.seamMean,
                                    seamMax: t.quality.seamMax,
                                    ok: t.quality.ok,
                                })),
                                wall: wall.map((f) => ({
                                    mapId: f.mapId,
                                    view: f.viewDirection,
                                    rot: f.rotate,
                                    x: f.coordinate.x,
                                    y: f.coordinate.y,
                                    z: f.coordinate.z,
                                })),
                                maps: mapsDump,
                            },
                            null,
                            2,
                        ),
                    );
                } catch {
                    /* ignore */
                }
                process.stdout.write(
                    JSON.stringify({
                        ok: false,
                        error: `quality ${quality.reason}`,
                        quality,
                        wallSize: wall.length,
                        wallView: wall[0]?.viewDirection,
                    }),
                );
                process.exit(3);
            }
            process.stdout.write(
                JSON.stringify({
                    ok: true,
                    pngBase64: png.toString('base64'),
                    wallSize: wall.length,
                    wallMapIds: wall.map((f) => f.mapId),
                    wallView: wall[0]?.viewDirection,
                    quality: {
                        cols: quality.cols,
                        rows: quality.rows,
                        seamMean: quality.seamMean,
                        blackRatio: quality.blackRatio,
                    },
                }),
            );
        })
        .catch((e) => {
            process.stdout.write(JSON.stringify({ ok: false, error: String(e?.message || e) }));
            process.exit(1);
        });
}

main();
