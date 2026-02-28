import { Plus } from "lucide-react";

import { formatUnixMs } from "@/app/helpers";
import type { ProgramSummary } from "@/app/types";
import { RowActionMenu } from "@/app/components/RowActionMenu";
import { Button } from "@/components/ui/button";
import { Card, CardHeader } from "@/components/ui/card";
import { Input } from "@/components/ui/input";

type ProgramListViewProps = {
  creatingProgram: boolean;
  onCreateProgram: () => void;
  programSearch: string;
  onProgramSearchChange: (value: string) => void;
  filteredPrograms: ProgramSummary[];
  onSelectProgram: (programId: string) => void;
  onDeleteProgram: (program: ProgramSummary) => void;
  deletingProgram: boolean;
};

export function ProgramListView({
  creatingProgram,
  onCreateProgram,
  programSearch,
  onProgramSearchChange,
  filteredPrograms,
  onSelectProgram,
  onDeleteProgram,
  deletingProgram
}: ProgramListViewProps) {
  return (
    <div className="space-y-4">
      <Card className="border-slate-200/80 bg-white/80 backdrop-blur">
        <CardHeader className="pb-3">
          <div className="text-xs uppercase tracking-[0.24em] text-slate-500">Workflow Registry</div>
          <div className="mt-1 text-2xl font-semibold tracking-tight">Programs</div>
          <div className="mt-1 text-sm text-muted-foreground">Store, version, and open workflows for editing.</div>
        </CardHeader>
      </Card>

      <section className="rounded-xl border border-slate-200/80 bg-white/80 p-4 backdrop-blur">
        <div className="mb-3 flex items-center justify-between gap-3">
          <Button onClick={onCreateProgram} disabled={creatingProgram}>
            <Plus className="mr-1 h-4 w-4" />
            {creatingProgram ? "Creating" : "Create Program"}
          </Button>
          <Input
            value={programSearch}
            onChange={(event) => onProgramSearchChange(event.target.value)}
            placeholder="Search by name..."
            className="h-9 w-full max-w-[320px]"
          />
        </div>

        <div className="overflow-hidden rounded-lg border">
          <div className="grid grid-cols-[minmax(220px,1.4fr)_120px_110px_170px_48px] gap-2 border-b bg-muted/40 px-3 py-2 text-[11px] uppercase tracking-wide text-muted-foreground">
            <div>Program</div>
            <div>Latest</div>
            <div>Versions</div>
            <div>Updated</div>
            <div className="text-right">Actions</div>
          </div>
          <div className="max-h-[66vh] overflow-auto">
            {filteredPrograms.map((program) => (
              <div
                key={program.program_id}
                className="grid w-full grid-cols-[minmax(220px,1.4fr)_120px_110px_170px_48px] items-center gap-2 border-b px-3 py-2 text-left text-sm transition hover:bg-muted/50"
              >
                <button
                  type="button"
                  onClick={() => onSelectProgram(program.program_id)}
                  className="col-span-4 grid min-w-0 grid-cols-[minmax(220px,1.4fr)_120px_110px_170px] items-center gap-2 text-left"
                >
                  <div className="truncate font-medium">{program.name}</div>
                  <div>v{program.latest_version}</div>
                  <div>{program.versions}</div>
                  <div className="text-xs text-muted-foreground">{formatUnixMs(program.updated_unix_ms)}</div>
                </button>
                <RowActionMenu
                  disabled={deletingProgram}
                  onDelete={() => onDeleteProgram(program)}
                />
              </div>
            ))}
            {filteredPrograms.length === 0 ? (
              <div className="px-3 py-6 text-center text-sm text-muted-foreground">No programs match your search.</div>
            ) : null}
          </div>
        </div>
      </section>
    </div>
  );
}
