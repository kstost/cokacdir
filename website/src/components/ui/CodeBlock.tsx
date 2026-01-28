import { useState } from 'react'
import { motion } from 'framer-motion'
import { Copy, Check } from 'lucide-react'

interface CodeBlockProps {
  code: string
  showCopy?: boolean
}

export default function CodeBlock({ code, showCopy = true }: CodeBlockProps) {
  const [copied, setCopied] = useState(false)

  const handleCopy = async () => {
    try {
      await navigator.clipboard.writeText(code)
      setCopied(true)
      setTimeout(() => setCopied(false), 2000)
    } catch {
      console.error('Failed to copy to clipboard')
    }
  }

  return (
    <motion.div
      className="relative group bg-bg-card border border-zinc-800 rounded-lg overflow-hidden"
      whileHover={{ borderColor: 'rgba(0, 212, 255, 0.3)' }}
    >
      <div className="flex items-center justify-between px-4 py-3">
        <code className="font-mono text-accent-cyan text-sm sm:text-base">
          <span className="text-zinc-500">$ </span>
          {code}
        </code>
        {showCopy && (
          <button
            onClick={handleCopy}
            className="ml-4 p-2 rounded-md bg-bg-elevated hover:bg-zinc-700 transition-colors"
            aria-label="Copy to clipboard"
          >
            {copied ? (
              <Check className="w-4 h-4 text-accent-green" />
            ) : (
              <Copy className="w-4 h-4 text-zinc-400 group-hover:text-white" />
            )}
          </button>
        )}
      </div>
    </motion.div>
  )
}
