import { motion } from 'framer-motion'
import { Columns, Bot, Keyboard, Eye, Zap, Activity } from 'lucide-react'
import Card from './ui/Card'

const features: { icon: typeof Zap; title: string; description: string; highlight?: boolean }[] = [
  {
    icon: Zap,
    title: 'Blazing Fast',
    description: 'Written in Rust for maximum performance. ~10ms startup, ~5MB memory usage, and a tiny ~4MB static binary with zero runtime dependencies.',
  },
  {
    icon: Bot,
    title: 'AI-Powered Commands',
    description: 'Natural language file operations powered by Claude AI. Describe what you want — move files, organize folders, find duplicates, all in plain language.',
    highlight: true,
  },
  {
    icon: Columns,
    title: 'Dual Panel',
    description: 'Classic two-panel layout for efficient file navigation and management.',
  },
  {
    icon: Keyboard,
    title: 'Keyboard Driven',
    description: 'Full keyboard navigation designed for power users. No mouse required.',
  },
  {
    icon: Eye,
    title: 'Built-in Viewer & Editor',
    description: 'View and edit files directly without leaving the application. Search functionality included.',
  },
  {
    icon: Activity,
    title: 'Process Manager',
    description: 'Monitor and manage system processes. Sort by CPU, memory, or PID. Kill processes with ease.',
  },
]

const containerVariants = {
  hidden: { opacity: 0 },
  visible: {
    opacity: 1,
    transition: {
      staggerChildren: 0.1,
    },
  },
}

const itemVariants = {
  hidden: { opacity: 0, y: 20 },
  visible: { opacity: 1, y: 0 },
}

export default function Features() {
  return (
    <section className="py-24 px-4" id="features">
      <div className="max-w-6xl mx-auto">
        {/* Section header */}
        <motion.div
          initial={{ opacity: 0, y: 20 }}
          whileInView={{ opacity: 1, y: 0 }}
          viewport={{ once: true }}
          transition={{ duration: 0.6 }}
          className="text-center mb-16"
        >
          <h2 className="text-3xl sm:text-4xl font-bold mb-4">
            <span className="gradient-text">Powerful Features</span>
          </h2>
          <p className="text-zinc-400 text-lg max-w-2xl mx-auto">
            Everything you need in a terminal file manager, and more.
          </p>
        </motion.div>

        {/* Features grid */}
        <motion.div
          variants={containerVariants}
          initial="hidden"
          whileInView="visible"
          viewport={{ once: true }}
          className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-6"
        >
          {features.map((feature, index) => (
            <motion.div key={index} variants={itemVariants} className={feature.highlight ? 'md:col-span-2 lg:col-span-1' : ''}>
              <Card className={`h-full ${feature.highlight ? 'ring-2 ring-accent-cyan/50 bg-gradient-to-br from-bg-card to-accent-cyan/5' : ''}`}>
                <div className={`w-12 h-12 rounded-lg flex items-center justify-center mb-4 ${feature.highlight ? 'bg-gradient-to-br from-accent-cyan/30 to-primary/30' : 'bg-gradient-to-br from-primary/20 to-accent-cyan/20'}`}>
                  <feature.icon className={`w-6 h-6 ${feature.highlight ? 'text-accent-cyan animate-pulse' : 'text-accent-cyan'}`} />
                </div>
                <h3 className="text-xl font-semibold mb-2">
                  {feature.highlight && <span className="text-accent-cyan">✨ </span>}
                  {feature.title}
                </h3>
                <p className={feature.highlight ? 'text-zinc-300' : 'text-zinc-400'}>{feature.description}</p>
              </Card>
            </motion.div>
          ))}
        </motion.div>
      </div>
    </section>
  )
}
