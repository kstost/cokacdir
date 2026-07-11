import { useState, ReactNode } from 'react'
import { Copy, Check, Info, AlertTriangle, Lightbulb } from 'lucide-react'

export function SectionTitle({ children }: { children: ReactNode }) {
  return <h1 className="text-3xl font-bold mb-6 gradient-text">{children}</h1>
}

export function SubSection({ title, children }: { title: string; children: ReactNode }) {
  return (
    <div className="mt-8">
      <h2 className="text-xl font-semibold text-white mb-4">{title}</h2>
      {children}
    </div>
  )
}

export function CodeBlock({ code }: { code: string }) {
  const [copied, setCopied] = useState(false)
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(code)
      setCopied(true)
      setTimeout(() => setCopied(false), 2000)
    } catch {
      // Keep the copy icon visible when clipboard access is unavailable.
      setCopied(false)
    }
  }
  return (
    <div className="relative group my-4">
      <pre className="bg-bg-card border border-zinc-800 rounded-lg p-4 overflow-x-auto">
        <code className="text-sm font-mono text-zinc-300 whitespace-pre-wrap break-all">{code}</code>
      </pre>
      <button
        onClick={copy}
        className="absolute top-2 right-2 p-1.5 rounded bg-zinc-700/50 hover:bg-zinc-600/50 opacity-0 group-hover:opacity-100 transition-opacity"
      >
        {copied ? <Check size={14} className="text-accent-green" /> : <Copy size={14} className="text-zinc-400" />}
      </button>
    </div>
  )
}

export function IC({ children }: { children: ReactNode }) {
  return (
    <code className="bg-bg-card border border-zinc-800 px-1.5 py-0.5 rounded text-sm font-mono text-accent-cyan">
      {children}
    </code>
  )
}

export function StepList({ children }: { children: ReactNode }) {
  return <div className="space-y-5 my-6">{children}</div>
}

export function StepItem({ number, title, children }: { number: number; title?: string; children: ReactNode }) {
  return (
    <div className="flex gap-4">
      <div className="flex-shrink-0 w-8 h-8 rounded-full bg-accent-cyan/20 text-accent-cyan flex items-center justify-center text-sm font-bold">
        {number}
      </div>
      <div className="flex-1 pt-0.5">
        {title && <h3 className="font-medium text-white mb-1">{title}</h3>}
        <div className="text-zinc-400 leading-relaxed">{children}</div>
      </div>
    </div>
  )
}

export function InfoBox({ type = 'info', children }: { type?: 'info' | 'warning' | 'tip'; children: ReactNode }) {
  const styles = {
    info: {
      border: 'border-primary/30',
      bg: 'bg-primary/5',
      icon: <Info size={16} className="text-primary-light" />,
      label: 'Info',
    },
    warning: {
      border: 'border-yellow-500/30',
      bg: 'bg-yellow-500/5',
      icon: <AlertTriangle size={16} className="text-yellow-400" />,
      label: 'Warning',
    },
    tip: {
      border: 'border-accent-green/30',
      bg: 'bg-accent-green/5',
      icon: <Lightbulb size={16} className="text-accent-green" />,
      label: 'Tip',
    },
  }
  const s = styles[type]
  return (
    <div className={`${s.border} ${s.bg} border rounded-lg p-4 my-4`}>
      <div className="flex items-center gap-2 mb-2 font-medium text-sm text-white">
        {s.icon} {s.label}
      </div>
      <div className="text-zinc-400 text-sm leading-relaxed">{children}</div>
    </div>
  )
}

export function CommandTable({ headers, rows }: { headers: string[]; rows: (string | ReactNode)[][] }) {
  return (
    <div className="overflow-x-auto my-4 border border-zinc-800 rounded-lg">
      <table className="w-full text-sm">
        <thead>
          <tr className="border-b border-zinc-700 bg-bg-card">
            {headers.map((h, i) => (
              <th key={i} className="text-left py-3 px-4 text-zinc-300 font-medium">
                {h}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row, i) => (
            <tr key={i} className="border-b border-zinc-800/50 last:border-0">
              {row.map((cell, j) => (
                <td key={j} className="py-2.5 px-4 text-zinc-400">
                  {cell}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}

export function P({ children }: { children: ReactNode }) {
  return <p className="text-zinc-400 leading-relaxed mb-4">{children}</p>
}

export function UL({ children }: { children: ReactNode }) {
  return <ul className="list-disc list-inside space-y-1.5 text-zinc-400 my-4 ml-2">{children}</ul>
}
