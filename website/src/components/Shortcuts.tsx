import { motion } from 'framer-motion'
import Card from './ui/Card'

const shortcutGroups = [
  {
    title: 'Navigation',
    shortcuts: [
      { key: '↑ ↓', action: 'Navigate files' },
      { key: 'Enter', action: 'Open directory' },
      { key: 'Esc', action: 'Parent directory' },
      { key: 'Tab', action: 'Switch panels' },
      { key: 'Home / End', action: 'First / Last item' },
    ],
  },
  {
    title: 'File Operations',
    shortcuts: [
      { key: 'c', action: 'Copy' },
      { key: 'm', action: 'Move' },
      { key: 'k', action: 'Create directory' },
      { key: 'x', action: 'Delete' },
      { key: 'r', action: 'Rename' },
    ],
  },
  {
    title: 'View & Tools',
    shortcuts: [
      { key: 'h', action: 'Help' },
      { key: 'o', action: 'File info' },
      { key: 'v', action: 'View file' },
      { key: 'e', action: 'Edit file' },
      { key: 'p', action: 'Process manager' },
    ],
  },
  {
    title: 'Selection & AI',
    shortcuts: [
      { key: 'Space', action: 'Select file' },
      { key: '*', action: 'Select all' },
      { key: 'n / s / d', action: 'Sort by name / size / date' },
      { key: '.', action: 'AI command' },
      { key: 'q', action: 'Quit' },
    ],
  },
]

export default function Shortcuts() {
  return (
    <section className="py-24 px-4" id="shortcuts">
      <div className="max-w-6xl mx-auto">
        <motion.div
          initial={{ opacity: 0, y: 20 }}
          whileInView={{ opacity: 1, y: 0 }}
          viewport={{ once: true }}
          transition={{ duration: 0.6 }}
          className="text-center mb-16"
        >
          <h2 className="text-3xl sm:text-4xl font-bold mb-4">
            <span className="gradient-text">Keyboard Shortcuts</span>
          </h2>
          <p className="text-zinc-400 text-lg max-w-2xl mx-auto">
            Master these shortcuts and navigate at the speed of thought.
          </p>
        </motion.div>

        <div className="grid grid-cols-1 md:grid-cols-2 gap-6">
          {shortcutGroups.map((group, groupIndex) => (
            <motion.div
              key={groupIndex}
              initial={{ opacity: 0, y: 20 }}
              whileInView={{ opacity: 1, y: 0 }}
              viewport={{ once: true }}
              transition={{ duration: 0.5, delay: groupIndex * 0.1 }}
            >
              <Card>
                <h3 className="text-lg font-semibold mb-4 text-accent-cyan">
                  {group.title}
                </h3>
                <div className="space-y-2">
                  {group.shortcuts.map((shortcut, index) => (
                    <div
                      key={index}
                      className="flex items-center justify-between py-2 border-b border-zinc-800 last:border-0"
                    >
                      <kbd className="px-2 py-1 bg-bg-elevated rounded text-sm font-mono text-white">
                        {shortcut.key}
                      </kbd>
                      <span className="text-zinc-400 text-sm">{shortcut.action}</span>
                    </div>
                  ))}
                </div>
              </Card>
            </motion.div>
          ))}
        </div>
      </div>
    </section>
  )
}
