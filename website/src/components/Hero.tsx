import { motion } from 'framer-motion'
import { Github, Terminal } from 'lucide-react'
import Button from './ui/Button'
import TerminalPreview from './TerminalPreview'

export default function Hero() {
  return (
    <section className="relative min-h-screen flex flex-col items-center justify-center px-4 py-20 overflow-hidden">
      {/* Animated grid background */}
      <div className="absolute inset-0 grid-background opacity-50" />

      {/* Gradient orbs */}
      <div className="absolute top-1/4 left-1/4 w-96 h-96 bg-primary/20 rounded-full blur-3xl animate-glow-pulse" />
      <div className="absolute bottom-1/4 right-1/4 w-96 h-96 bg-accent-cyan/20 rounded-full blur-3xl animate-glow-pulse" style={{ animationDelay: '2s' }} />

      <div className="relative z-10 max-w-6xl mx-auto text-center">
        {/* Badge */}
        <motion.div
          initial={{ opacity: 0, y: 20 }}
          animate={{ opacity: 1, y: 0 }}
          transition={{ duration: 0.6 }}
          className="inline-flex items-center gap-2 px-4 py-2 rounded-full bg-gradient-to-r from-accent-cyan/20 to-primary/20 border border-accent-cyan/50 mb-8"
        >
          <span className="text-lg">🤖</span>
          <span className="text-sm text-accent-cyan font-semibold">Powered by Claude AI — Natural Language File Management</span>
        </motion.div>

        {/* Main title */}
        <motion.h1
          initial={{ opacity: 0, y: 20 }}
          animate={{ opacity: 1, y: 0 }}
          transition={{ duration: 0.8, delay: 0.1 }}
          className="text-5xl sm:text-6xl lg:text-7xl font-extrabold mb-6"
        >
          <span className="gradient-text">cokacdir</span>
        </motion.h1>

        {/* Tagline */}
        <motion.p
          initial={{ opacity: 0, y: 20 }}
          animate={{ opacity: 1, y: 0 }}
          transition={{ duration: 0.8, delay: 0.2 }}
          className="text-2xl sm:text-3xl lg:text-4xl font-bold text-white mb-4"
        >
          Terminal File Manager
          <br />
          <span className="text-glow text-accent-cyan">for Vibe Coders</span>
        </motion.p>

        {/* Sub-tagline */}
        <motion.p
          initial={{ opacity: 0, y: 20 }}
          animate={{ opacity: 1, y: 0 }}
          transition={{ duration: 0.8, delay: 0.25 }}
          className="text-lg text-zinc-400 mb-4"
        >
          An easy terminal explorer for vibe coders who are scared of the terminal
        </motion.p>

        {/* Subtitle */}
        <motion.p
          initial={{ opacity: 0, y: 20 }}
          animate={{ opacity: 1, y: 0 }}
          transition={{ duration: 0.8, delay: 0.3 }}
          className="text-lg text-zinc-400 max-w-2xl mx-auto mb-10"
        >
          Just press <kbd className="px-2 py-1 bg-zinc-800 rounded text-accent-cyan font-mono">.</kbd> and tell the AI what you need.
          <br />
          <span className="text-zinc-500">Move files, organize folders, find duplicates — all in plain language.</span>
        </motion.p>

        {/* CTA buttons */}
        <motion.div
          initial={{ opacity: 0, y: 20 }}
          animate={{ opacity: 1, y: 0 }}
          transition={{ duration: 0.8, delay: 0.4 }}
          className="flex flex-col sm:flex-row gap-4 justify-center items-center mb-16"
        >
          <Button href="#install">
            <Terminal className="w-5 h-5" />
            curl ... | bash
          </Button>
          <Button variant="secondary" href="https://github.com/kstost/cokacdir">
            <Github className="w-5 h-5" />
            View on GitHub
          </Button>
        </motion.div>

        {/* Terminal preview */}
        <motion.div
          initial={{ opacity: 0, scale: 0.95 }}
          animate={{ opacity: 1, scale: 1 }}
          transition={{ duration: 1, delay: 0.6 }}
        >
          <TerminalPreview />
        </motion.div>
      </div>

      {/* Scroll indicator */}
      <motion.div
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        transition={{ delay: 1.5 }}
        className="absolute bottom-8 left-1/2 -translate-x-1/2"
        aria-hidden="true"
      >
        <motion.div
          animate={{ y: [0, 8, 0] }}
          transition={{ duration: 1.5, repeat: Infinity }}
          className="w-6 h-10 border-2 border-zinc-600 rounded-full flex justify-center pt-2"
        >
          <div className="w-1.5 h-1.5 bg-accent-cyan rounded-full" />
        </motion.div>
      </motion.div>
    </section>
  )
}
