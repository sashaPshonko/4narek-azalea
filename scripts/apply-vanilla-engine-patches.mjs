/**
 * Повторно накатывает vanilla-правки на node_modules после npm i:
 * - mineflayer physics: 1 tick / fire (без wall-clock catchup)
 * - prismarine-physics: float accel 0.16277136 + fround heading/friction
 * - minecraft-data 1.21.11: attribute_modifiers как в TLauncher/Java (display per entry)
 *
 * Usage: node scripts/apply-vanilla-engine-patches.mjs
 */
import fs from 'fs';
import { createRequire } from 'module';
import { dirname, join } from 'path';
import { fileURLToPath } from 'url';

const require = createRequire(import.meta.url);
const root = join(dirname(fileURLToPath(import.meta.url)), '..');

function resolvePkg(name) {
    return dirname(require.resolve(`${name}/package.json`));
}

/** Как ItemAttributeModifiers$Entry.STREAM_CODEC в клиенте 1.21.11 (TLauncher jar). */
const ATTR_MODIFIERS_VANILLA = [
    'array',
    {
        countType: 'varint',
        type: [
            'container',
            [
                { name: 'typeId', type: 'varint' },
                { name: 'name', type: 'string' },
                { name: 'value', type: 'f64' },
                {
                    name: 'operation',
                    type: [
                        'mapper',
                        {
                            type: 'varint',
                            mappings: {
                                0: 'add',
                                1: 'multiply_base',
                                2: 'multiply_total',
                            },
                        },
                    ],
                },
                {
                    name: 'slot',
                    type: [
                        'mapper',
                        {
                            type: 'varint',
                            mappings: {
                                0: 'any',
                                1: 'main_hand',
                                2: 'off_hand',
                                3: 'hand',
                                4: 'feet',
                                5: 'legs',
                                6: 'chest',
                                7: 'head',
                                8: 'armor',
                                9: 'body',
                                10: 'saddle',
                            },
                        },
                    ],
                },
                {
                    name: 'display',
                    type: [
                        'container',
                        [
                            {
                                name: 'type',
                                type: [
                                    'mapper',
                                    {
                                        type: 'varint',
                                        mappings: {
                                            0: 'default',
                                            1: 'hidden',
                                            2: 'override',
                                        },
                                    },
                                ],
                            },
                            {
                                name: 'component',
                                type: [
                                    'switch',
                                    {
                                        compareTo: 'type',
                                        fields: {
                                            default: 'void',
                                            hidden: 'void',
                                            override: 'anonymousNbt',
                                        },
                                        default: 'void',
                                    },
                                ],
                            },
                        ],
                    ],
                },
            ],
        ],
    },
];

function patchAttributeModifiersProtocol() {
    const protoPath = join(
        resolvePkg('minecraft-data'),
        'minecraft-data/data/pc/1.21.11/protocol.json',
    );
    if (!fs.existsSync(protoPath)) {
        console.warn('[miss] minecraft-data 1.21.11 protocol.json');
        return 0;
    }
    const proto = JSON.parse(fs.readFileSync(protoPath, 'utf8'));
    const fields = proto?.types?.SlotComponent?.[1]?.[1]?.type?.[1]?.fields;
    if (!fields?.attribute_modifiers) {
        console.warn('[miss] SlotComponent.attribute_modifiers');
        return 0;
    }
    const cur = fields.attribute_modifiers;
    const already =
        Array.isArray(cur) &&
        cur[0] === 'array' &&
        JSON.stringify(cur[1]?.type?.[1]?.map((f) => f.name)) ===
            JSON.stringify(['typeId', 'name', 'value', 'operation', 'slot', 'display']);
    if (already) {
        // всё равно дожмём void defaults на display switch
        const entry = cur[1]?.type?.[1];
        const disp = entry?.find((f) => f.name === 'display');
        const sw = disp?.type?.[1]?.find((f) => f.name === 'component')?.type?.[1];
        if (sw && sw.fields?.default === 'void' && sw.default === 'void') {
            console.log('[skip] minecraft-data: attribute_modifiers (already vanilla)');
            return 0;
        }
    }
    fields.attribute_modifiers = ATTR_MODIFIERS_VANILLA;
    fs.writeFileSync(protoPath, JSON.stringify(proto));
    console.log('[ok] minecraft-data: attribute_modifiers → TLauncher/Java per-entry display');
    return 1;
}

function patchFile(filePath, replacements, label) {
    let src = fs.readFileSync(filePath, 'utf8');
    let n = 0;
    for (const { from, to, id, already } of replacements) {
        const marker = already || to;
        // Уникальный маркер патча — не первые символы `to` (они часто совпадают со stock).
        if (src.includes(marker) && !src.includes(from)) {
            console.log(`[skip] ${label}: ${id} (already)`);
            continue;
        }
        if (!src.includes(from)) {
            console.warn(`[miss] ${label}: ${id}`);
            continue;
        }
        src = src.replace(from, to);
        n++;
        console.log(`[ok] ${label}: ${id}`);
    }
    if (n) fs.writeFileSync(filePath, src);
    return n;
}

const mfPhysics = join(resolvePkg('mineflayer'), 'lib/plugins/physics.js');
const ppIndex = join(resolvePkg('prismarine-physics'), 'index.js');

const doPhysicsOld = `  // This function should be executed each tick (every 0.05 seconds)
  // How it works: https://gafferongames.com/post/fix_your_timestep/

  // WARNING: THIS IS NOT ACCURATE ON WINDOWS (15.6 Timer Resolution)
  // use WSL or switch to Linux
  // see: https://discord.com/channels/413438066984747026/519952494768685086/901948718255833158
  let timeAccumulator = 0
  let catchupTicks = 0
  function doPhysics () {
    const now = performance.now()
    const deltaSeconds = (now - lastPhysicsFrameTime) / 1000
    lastPhysicsFrameTime = now

    timeAccumulator += deltaSeconds
    catchupTicks = 0
    while (timeAccumulator >= PHYSICS_TIMESTEP) {
      tickPhysics(now)
      timeAccumulator -= PHYSICS_TIMESTEP
      catchupTicks++
      if (catchupTicks >= PHYSICS_CATCHUP_TICKS) break
    }
  }`;

const doPhysicsNew = `  // 20 TPS: ровно один клиентский тик на срабатывание таймера.
  // Пачку catchup по wall-clock не гоняем — иначе рваный motion ≠ ваниль / FunAC.
  // (Точный schedule: lib/vanilla-tick.mjs подменяет setInterval(50) из этого файла.)
  let timeAccumulator = 0
  let catchupTicks = 0
  function doPhysics () {
    const now = performance.now()
    lastPhysicsFrameTime = now
    timeAccumulator = 0
    catchupTicks = 0
    tickPhysics(now)
  }`;

const headingOld = `  function applyHeading (entity, strafe, forward, multiplier) {
    let speed = Math.sqrt(strafe * strafe + forward * forward)
    if (speed < 0.01) return new Vec3(0, 0, 0)

    speed = multiplier / Math.max(speed, 1)

    strafe *= speed
    forward *= speed

    const yaw = Math.PI - entity.yaw
    const sin = Math.sin(yaw)
    const cos = Math.cos(yaw)

    const vel = entity.vel
    vel.x -= strafe * cos + forward * sin
    vel.z += forward * cos - strafe * sin
  }`;

const headingNew = `  function applyHeading (entity, strafe, forward, multiplier) {
    let speed = Math.sqrt(strafe * strafe + forward * forward)
    if (speed < 0.01) return new Vec3(0, 0, 0)

    speed = multiplier / Math.max(speed, 1)

    strafe *= speed
    forward *= speed

    const yaw = Math.PI - entity.yaw
    const sin = Math.sin(yaw)
    const cos = Math.cos(yaw)

    const vel = entity.vel
    // JVM float: LivingEntity.travel heading
    vel.x = Math.fround(vel.x - Math.fround(strafe * cos + forward * sin))
    vel.z = Math.fround(vel.z + Math.fround(forward * cos - strafe * sin))
  }`;

const accelOld = `        // Calculate what the speed is (0.1 if no modification)
        const attributeSpeed = attribute.getAttributeValue(playerSpeedAttribute)
        inertia = (blockSlipperiness[blockUnder.type] || physics.defaultSlipperiness) * 0.91
        acceleration = attributeSpeed * (0.1627714 / (inertia * inertia * inertia))
        if (acceleration < 0) acceleration = 0 // acceleration should not be negative
      } else {
        acceleration = physics.airborneAcceleration
        inertia = physics.airborneInertia

        if (entity.control.sprint) {
          const airSprintFactor = physics.airborneAcceleration * 0.3
          acceleration += airSprintFactor
        }
      }

      applyHeading(entity, strafe, forward, acceleration)

      if (isOnLadder(world, pos)) {
        vel.x = math.clamp(-physics.ladderMaxSpeed, vel.x, physics.ladderMaxSpeed)
        vel.z = math.clamp(-physics.ladderMaxSpeed, vel.z, physics.ladderMaxSpeed)
        vel.y = Math.max(vel.y, entity.control.sneak ? 0 : -physics.ladderMaxSpeed)
      }

      moveEntity(entity, world, vel.x, vel.y, vel.z)

      if (isOnLadder(world, pos) && (entity.isCollidedHorizontally ||
        (supportFeature('climbUsingJump') && entity.control.jump))) {
        vel.y = physics.ladderClimbSpeed // climb ladder
      }

      // Apply friction and gravity
      if (entity.levitation > 0) {
        vel.y += (0.05 * entity.levitation - vel.y) * 0.2
      } else {
        vel.y -= physics.gravity * gravityMultiplier
      }
      vel.y *= physics.airdrag
      vel.x *= inertia
      vel.z *= inertia
    }
  }`;

const accelNew = `        // Calculate what the speed is (0.1 if no modification)
        const attributeSpeed = attribute.getAttributeValue(playerSpeedAttribute)
        // slip * 0.91f as float; accel factor 0.16277136f (yarn LivingEntity)
        inertia = Math.fround((blockSlipperiness[blockUnder.type] || physics.defaultSlipperiness) * 0.91)
        const inertia3 = Math.fround(Math.fround(inertia * inertia) * inertia)
        acceleration = Math.fround(attributeSpeed * Math.fround(Math.fround(0.16277136) / inertia3))
        if (acceleration < 0) acceleration = 0 // acceleration should not be negative
      } else {
        acceleration = physics.airborneAcceleration
        inertia = physics.airborneInertia

        if (entity.control.sprint) {
          const airSprintFactor = physics.airborneAcceleration * 0.3
          acceleration += airSprintFactor
        }
      }

      applyHeading(entity, strafe, forward, acceleration)

      if (isOnLadder(world, pos)) {
        vel.x = math.clamp(-physics.ladderMaxSpeed, vel.x, physics.ladderMaxSpeed)
        vel.z = math.clamp(-physics.ladderMaxSpeed, vel.z, physics.ladderMaxSpeed)
        vel.y = Math.max(vel.y, entity.control.sneak ? 0 : -physics.ladderMaxSpeed)
      }

      moveEntity(entity, world, vel.x, vel.y, vel.z)

      if (isOnLadder(world, pos) && (entity.isCollidedHorizontally ||
        (supportFeature('climbUsingJump') && entity.control.jump))) {
        vel.y = physics.ladderClimbSpeed // climb ladder
      }

      // Apply friction and gravity (float like Entity.travel)
      if (entity.levitation > 0) {
        vel.y += (0.05 * entity.levitation - vel.y) * 0.2
      } else {
        vel.y = Math.fround(vel.y - Math.fround(physics.gravity * gravityMultiplier))
      }
      vel.y = Math.fround(vel.y * physics.airdrag)
      vel.x = Math.fround(vel.x * inertia)
      vel.z = Math.fround(vel.z * inertia)
      pos.x = Math.fround(pos.x)
      pos.y = Math.fround(pos.y)
      pos.z = Math.fround(pos.z)
    }
  }`;

let total = 0;
total += patchFile(
    mfPhysics,
    [{
        from: doPhysicsOld,
        to: doPhysicsNew,
        id: 'single-tick doPhysics',
        already: 'ровно один клиентский тик на срабатывание таймера',
    }, {
        from: `  function tickPhysics (now) {
    if (!bot.entity?.position || !Number.isFinite(bot.entity.position.x)) return // entity not ready
    if (bot.blockAt(bot.entity.position) == null) return // check if chunk is unloaded
    if (bot.physicsEnabled && shouldUsePhysics) {
      physics.simulatePlayer(new PlayerState(bot, controlState), world).apply(bot)
      bot.emit('physicsTick')
      bot.emit('physicTick') // Deprecated, only exists to support old plugins. May be removed in the future
    }
    if (shouldUsePhysics) {
      updatePosition(now)
    }
  }`,
        to: `  function tickPhysics (now) {
    if (!bot.entity?.position || !Number.isFinite(bot.entity.position.x)) return // entity not ready
    // FunTime/TLauncher: тик и position идут даже без чанка. Stock mineflayer
    // делал return → нет physicsTick → FunAC/команды глухие (4narek vanilla patch).
    const chunkReady = bot.blockAt(bot.entity.position) != null
    if (bot.physicsEnabled && shouldUsePhysics && chunkReady) {
      physics.simulatePlayer(new PlayerState(bot, controlState), world).apply(bot)
    }
    if (shouldUsePhysics) {
      bot.emit('physicsTick')
      bot.emit('physicTick') // Deprecated, only exists to support old plugins. May be removed in the future
      updatePosition(now)
    }
  }`,
        id: 'tickPhysics without chunk early-return',
        already: 'тик и position идут даже без чанка',
    }],
    'mineflayer/physics.js',
);
total += patchFile(
    ppIndex,
    [
        {
            from: headingOld,
            to: headingNew,
            id: 'fround applyHeading',
            already: 'JVM float: LivingEntity.travel heading',
        },
        {
            from: accelOld,
            to: accelNew,
            id: 'float ground accel+friction',
            already: '0.16277136',
        },
    ],
    'prismarine-physics',
);

total += patchAttributeModifiersProtocol();

console.log(total ? `patched ${total} hunk(s)` : 'nothing to patch (already applied or upstream changed)');
