import { motion } from "framer-motion";

/**
 * 二次元背景装饰层（参考 ai_town AnimeBackground）：
 * 三色动态光晕（sakura 粉 / sky 天蓝 / twilight 暮紫）+ 网格点阵。
 * 花瓣飘落由 index.css 的 `.petal` 层承担，两者共用 `-z` 背景层不挡交互。
 */
export function AnimeBackground() {
  return (
    <div className="pointer-events-none fixed inset-0 -z-10 overflow-hidden">
      {/* 动态光晕：浅色为粉奶二次元，深色下极淡星光（dark: class variant） */}
      <motion.div
        className="absolute left-[-10%] top-[-20%] h-[70vw] w-[70vw] rounded-full bg-sakura-200/30 blur-[120px] dark:bg-white/2"
        animate={{ x: [0, 50, 0], y: [0, 30, 0] }}
        transition={{ duration: 20, repeat: Infinity, ease: "easeInOut" }}
      />
      <motion.div
        className="absolute bottom-[-20%] right-[-10%] h-[60vw] w-[60vw] rounded-full bg-[#ffe9ef]/40 blur-[120px] dark:bg-[#d8d8e2]/2"
        animate={{ x: [0, -40, 0], y: [0, -40, 0] }}
        transition={{ duration: 25, repeat: Infinity, ease: "easeInOut" }}
      />
      <motion.div
        className="absolute left-[30%] top-[40%] h-[40vw] w-[40vw] rounded-full bg-[#ffd9e4]/35 blur-[100px] dark:bg-white/2"
        animate={{ scale: [1, 1.1, 1] }}
        transition={{ duration: 15, repeat: Infinity, ease: "easeInOut" }}
      />

      {/* 网格点阵：浅色樱花粉点，深色青色数据点 */}
      <div
        className="dot-grid absolute inset-0 opacity-[0.05]"
        style={{ backgroundSize: "40px 40px" }}
      />

      {/* 飘落花瓣（10 片，CSS 动画，见 index.css `.petal`） */}
      {[...Array(10)].map((_, i) => (
        <div key={i} className="petal" />
      ))}
    </div>
  );
}
