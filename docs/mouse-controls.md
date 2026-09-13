# Mouse Controls

The file manager, built-in code editor, and text viewer accept mouse input in terminals that provide mouse reporting. Mouse capture starts with the TUI and is released on exit and while an external terminal command runs.

## File panels

| Gesture | Action |
| --- | --- |
| Vertical wheel over a file list | Scroll that list three rows; keep the keyboard focus and file cursor |
| Left click a file | Focus its panel and place the file cursor on that row |
| Double click a file or directory | Run the existing Open action |
| Ctrl + left click | Toggle the file's selection mark |
| Shift + left click | Select the range from the previous file cursor to the clicked row |
| Drag a file to another panel and release | Copy to that panel's current directory |
| Hold Shift when releasing a drag in another panel | Move to that panel's current directory |

Dragging a marked file includes the marked files. Dragging an unmarked file uses that file. The destination is the other panel's **current directory**, including when the pointer is over a directory row; dropping within the source panel does nothing. The parent entry (`..`) cannot be dragged.

Drops use the existing clipboard/paste operation, including overwrite dialogs, progress, cancellation, local source identity checks, and cross-volume move settings. A valid drop replaces the application's file clipboard. Remote copies use the existing SSH/SFTP transfer support; remote moves retain the same restrictions as keyboard Cut/Paste.

The file-list header, border, and scrollbar are outside the mouse target. Scrolling alone does not select files. Keyboard navigation brings the file cursor back into view. Escape, other keys, a screen or terminal-size change, and focus loss cancel a pending drag. Dialogs and active operations block background mouse actions.

## Code editor

| Gesture | Action |
| --- | --- |
| Left click text | Place the caret |
| Hold left button and drag | Select text in either direction, across lines |
| Shift + left click | Select from the previous caret to the clicked position |
| Drag beyond the text area's edge | Continue selecting with automatic scrolling |
| Vertical wheel | Scroll three visual rows without moving the caret or changing selection |
| Horizontal wheel, with wrapping off | Scroll horizontally |

Coordinates account for line numbers, tabs, wide characters such as Korean text, horizontal scrolling, and wrapped lines. Drag selection works with the existing copy/cut/paste and editing commands. Mouse gestures themselves do not change the document or undo history. Find/replace, Go to Line, and exit confirmation keep their keyboard controls.

## Text viewer and other screens

The text viewer supports vertical wheel scrolling and horizontal wheel scrolling with wrapping off. Its search and Go to Line prompts keep their keyboard controls. Other screens and dialogs continue to use their existing keyboard controls.

## Running inside cokacmux

Use a version of cokacmux that forwards button presses, drags, and releases to mouse-enabled child applications. Its earlier wheel-only support is insufficient for clicking or selecting text. A gesture stays with the PTY where it began, with coordinates translated into that PTY's content area.

Some terminals reserve Shift + mouse for terminal text selection. Use the terminal's mouse-reporting settings, or keyboard Cut/Paste for moves when Shift is intercepted. Terminal selection and the editor's own selection are separate; use your terminal's capture-bypass gesture when selecting terminal output. Keyboard controls remain available in terminals without mouse reporting.
