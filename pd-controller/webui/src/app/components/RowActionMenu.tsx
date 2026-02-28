import { MoreHorizontal, Trash2 } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";

import { Button } from "@/components/ui/button";

type RowActionMenuProps = {
  onDelete: () => void;
  deleteLabel?: string;
  disabled?: boolean;
};

export function RowActionMenu({ onDelete, deleteLabel = "Delete", disabled = false }: RowActionMenuProps) {
  const [open, setOpen] = useState(false);
  const [menuPosition, setMenuPosition] = useState<{ top: number; left: number } | null>(null);
  const rootRef = useRef<HTMLDivElement | null>(null);
  const buttonRef = useRef<HTMLButtonElement | null>(null);
  const menuRef = useRef<HTMLDivElement | null>(null);

  const updateMenuPosition = useCallback(() => {
    const button = buttonRef.current;
    if (!button) {
      return;
    }
    const rect = button.getBoundingClientRect();
    const menuWidth = 140;
    const menuHeight = 42;
    let left = rect.right - menuWidth;
    left = Math.max(8, Math.min(left, window.innerWidth - menuWidth - 8));
    let top = rect.bottom + 6;
    if (top + menuHeight > window.innerHeight - 8) {
      top = Math.max(8, rect.top - menuHeight - 6);
    }
    setMenuPosition({ top, left });
  }, []);

  useEffect(() => {
    const onPointerDown = (event: MouseEvent) => {
      const target = event.target as Node;
      if (!rootRef.current) {
        return;
      }
      if (rootRef.current.contains(target)) {
        return;
      }
      if (menuRef.current?.contains(target)) {
        return;
      }
      setOpen(false);
    };
    document.addEventListener("mousedown", onPointerDown);
    return () => document.removeEventListener("mousedown", onPointerDown);
  }, []);

  useEffect(() => {
    if (!open) {
      return;
    }
    updateMenuPosition();
    const onViewportChange = () => {
      if (!buttonRef.current) {
        setOpen(false);
        return;
      }
      updateMenuPosition();
    };
    window.addEventListener("resize", onViewportChange);
    window.addEventListener("scroll", onViewportChange, true);
    return () => {
      window.removeEventListener("resize", onViewportChange);
      window.removeEventListener("scroll", onViewportChange, true);
    };
  }, [open, updateMenuPosition]);

  return (
    <div ref={rootRef} className="relative flex justify-end">
      <Button
        ref={buttonRef}
        type="button"
        size="sm"
        variant="ghost"
        className="h-7 w-7 p-0"
        disabled={disabled}
        aria-label="Row actions"
        onClick={(event) => {
          event.stopPropagation();
          event.preventDefault();
          setOpen((value) => {
            const next = !value;
            if (next) {
              updateMenuPosition();
            }
            return next;
          });
        }}
      >
        <MoreHorizontal className="h-4 w-4" />
      </Button>
      {open && menuPosition
        ? createPortal(
            <div
              ref={menuRef}
              style={{ top: menuPosition.top, left: menuPosition.left }}
              className="fixed z-[100] min-w-[140px] rounded-md border bg-white p-1 shadow-lg"
            >
              <button
                type="button"
                className="flex w-full items-center gap-2 rounded px-2 py-1.5 text-left text-sm text-rose-700 transition hover:bg-rose-50"
                onClick={(event) => {
                  event.stopPropagation();
                  event.preventDefault();
                  setOpen(false);
                  onDelete();
                }}
              >
                <Trash2 className="h-4 w-4" />
                {deleteLabel}
              </button>
            </div>,
            document.body
          )
        : null}
    </div>
  );
}
