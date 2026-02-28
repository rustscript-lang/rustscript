import { Code2, Maximize2, Minimize2 } from "lucide-react";

import { HighlightedCode } from "@/app/components/HighlightedCode";
import type { SourceFlavor, UiSourceBundle } from "@/app/types";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";

type GeneratedCodePanelProps = {
  floating?: boolean;
  minimized?: boolean;
  rendering: boolean;
  activeFlavor: SourceFlavor;
  source: UiSourceBundle;
  onFlavorChange: (value: SourceFlavor) => void;
  onToggleMinimized?: () => void;
  onEdit?: () => void;
  readOnly?: boolean;
  enableLint?: boolean;
  onCodeChange?: (flavor: SourceFlavor, value: string) => void;
  editorHeight?: string;
  description?: string;
  title?: string;
  showHeader?: boolean;
};

export function GeneratedCodePanel({
  floating = false,
  minimized = false,
  rendering,
  activeFlavor,
  source,
  onFlavorChange,
  onToggleMinimized,
  onEdit,
  readOnly = true,
  enableLint = false,
  onCodeChange,
  editorHeight = "min(56vh, 640px)",
  description,
  title = "Generated Code",
  showHeader = true
}: GeneratedCodePanelProps) {
  const resolvedEditorHeight = floating ? "100%" : editorHeight;
  const minimizedFloating = floating && minimized;

  if (minimizedFloating) {
    return (
      <Card className="pointer-events-auto h-10 w-10 border-slate-700 bg-white/95 shadow-lg backdrop-blur">
        <div className="flex h-full items-center justify-center">
          <Button
            size="sm"
            variant="ghost"
            className="h-8 w-8 px-0"
            onClick={onToggleMinimized}
            aria-label="Expand generated code panel"
          >
            <Code2 className="h-4 w-4" />
          </Button>
        </div>
      </Card>
    );
  }

  return (
    <Card
      className={
        floating
          ? "pointer-events-auto flex h-full min-h-0 w-[440px] flex-col overflow-hidden border-slate-700 bg-white/95 backdrop-blur transition-[height,transform,box-shadow] duration-300 ease-out"
          : "border-slate-200/80 bg-white/90 shadow-xl backdrop-blur"
      }
    >
      {showHeader ? (
        <CardHeader className="pb-3">
          <div className="flex items-start justify-between gap-3">
            <div>
              <CardTitle>{title}</CardTitle>
              {description ? <div className="text-sm text-muted-foreground">{description}</div> : null}
            </div>
            <div className="flex items-start gap-1 self-start">
              {onEdit ? (
                <Button size="sm" variant="outline" className="-mt-0.5 h-7 px-2 text-xs" onClick={onEdit}>
                  Edit
                </Button>
              ) : null}
              {floating ? (
                <Button
                  size="sm"
                  variant="ghost"
                  className="h-7 w-7 px-0"
                  onClick={onToggleMinimized}
                  aria-label={minimized ? "Expand generated code panel" : "Minimize generated code panel"}
                >
                  {minimized ? (
                    <Maximize2 className="h-3.5 w-3.5 transition-transform duration-300 ease-out" />
                  ) : (
                    <Minimize2 className="h-3.5 w-3.5 transition-transform duration-300 ease-out" />
                  )}
                </Button>
              ) : null}
            </div>
          </div>
        </CardHeader>
      ) : null}
      <div
        className={
          floating
            ? "grid min-h-0 flex-1 grid-rows-[1fr] transition-[grid-template-rows,opacity] duration-300 ease-out"
            : "grid grid-rows-[1fr]"
        }
      >
        <div className="h-full min-h-0 overflow-hidden">
          <CardContent className={floating ? "flex h-full min-h-0 flex-col overflow-hidden" : ""}>
            <Tabs
              value={activeFlavor}
              onValueChange={(value) => onFlavorChange(value as SourceFlavor)}
              className={floating ? "flex h-full min-h-0 flex-col" : ""}
            >
              <TabsList className="grid w-full shrink-0 grid-cols-4">
                <TabsTrigger value="rustscript">RustScript</TabsTrigger>
                <TabsTrigger value="javascript">JavaScript</TabsTrigger>
                <TabsTrigger value="lua">Lua</TabsTrigger>
                <TabsTrigger value="scheme">Scheme</TabsTrigger>
              </TabsList>
              <TabsContent
                value="rustscript"
                className={floating ? "min-h-0 flex-1 overflow-hidden data-[state=active]:flex data-[state=active]:flex-col" : ""}
              >
                <HighlightedCode
                  flavor="rustscript"
                  source={source}
                  readOnly={readOnly}
                  enableLint={enableLint}
                  height={resolvedEditorHeight}
                  onChange={(value) => onCodeChange?.("rustscript", value)}
                />
              </TabsContent>
              <TabsContent
                value="javascript"
                className={floating ? "min-h-0 flex-1 overflow-hidden data-[state=active]:flex data-[state=active]:flex-col" : ""}
              >
                <HighlightedCode
                  flavor="javascript"
                  source={source}
                  readOnly={readOnly}
                  enableLint={enableLint}
                  height={resolvedEditorHeight}
                  onChange={(value) => onCodeChange?.("javascript", value)}
                />
              </TabsContent>
              <TabsContent
                value="lua"
                className={floating ? "min-h-0 flex-1 overflow-hidden data-[state=active]:flex data-[state=active]:flex-col" : ""}
              >
                <HighlightedCode
                  flavor="lua"
                  source={source}
                  readOnly={readOnly}
                  enableLint={enableLint}
                  height={resolvedEditorHeight}
                  onChange={(value) => onCodeChange?.("lua", value)}
                />
              </TabsContent>
              <TabsContent
                value="scheme"
                className={floating ? "min-h-0 flex-1 overflow-hidden data-[state=active]:flex data-[state=active]:flex-col" : ""}
              >
                <HighlightedCode
                  flavor="scheme"
                  source={source}
                  readOnly={readOnly}
                  enableLint={enableLint}
                  height={resolvedEditorHeight}
                  onChange={(value) => onCodeChange?.("scheme", value)}
                />
              </TabsContent>
            </Tabs>
          </CardContent>
        </div>
      </div>
    </Card>
  );
}
