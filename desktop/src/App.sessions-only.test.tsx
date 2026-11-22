import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";
import SendPage from "./features/send/SendPage";
import ReceivePage from "./features/receive/ReceivePage";
import { I18nProvider } from "./lib/i18n";
import { inventory, preview, basePlan } from "./test/fixtures";

const api = vi.hoisted(() => ({
  createPackage: vi.fn(), openPath: vi.fn(), applyRestore: vi.fn(),
  buildRestorePlan: vi.fn(), inspectPackage: vi.fn(), openRestoredThread: vi.fn(),
  selectRestoreDestinations: vi.fn(), prepareSupport: vi.fn(),
}));
vi.mock("./lib/api", () => api);
const noop = () => {};
const headingRef = { current: null };

beforeEach(() => {
  vi.resetAllMocks();
  window.localStorage.clear();
  api.createPackage.mockResolvedValue(null);
  api.buildRestorePlan.mockResolvedValue(basePlan);
});

it("exports only conversations while retaining project identity and hiding optional files", async () => {
  const user = userEvent.setup();
  render(<I18nProvider><SendPage headingRef={headingRef} inventory={inventory}
    onOperationStart={noop} onOperationEnd={noop} onHome={noop} /></I18nProvider>);
  await user.click(screen.getByRole("checkbox", { name: /只迁移会话/ }));
  expect(screen.queryByText("迁移你希望在新电脑继续使用的能力")).not.toBeInTheDocument();
  await user.click(screen.getByRole("checkbox", { name: "选择项目 rehome-app" }));
  await user.click(screen.getByRole("button", { name: "继续" }));
  expect(screen.getByText("只迁移会话与项目归属，项目文件不会进入迁移包。")).toBeVisible();
  expect(screen.queryByText(/勾选项目会包含项目文件/)).not.toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "选择保存位置并创建" }));
  expect(api.createPackage).toHaveBeenCalledWith({
    include_project_files: false, project_ids: [],
    conversation_ids: [inventory.conversations[0].task_id],
    skill_ids: [], plugin_ids: [], generated_image_ids: [],
  });
});

it("previews selected existing project folders rather than the unused project-copy root", async () => {
  const user = userEvent.setup();
  const project = inventory.projects[0];
  api.inspectPackage.mockResolvedValue({ ...preview, manifest: { ...preview.manifest, mode: "sessions_only", projects: [project] } });
  const destination = "/Users/example/Code/existing-project";
  api.selectRestoreDestinations.mockResolvedValueOnce(null).mockResolvedValue({
    selection_id: "destinations", target_codex_home: inventory.codex_home,
    projects_root: "unused-project-copy-root", backup_root: "backups",
    project_destinations: { [project.project_id]: destination },
  });
  render(<I18nProvider><ReceivePage headingRef={headingRef} inventory={inventory}
    rollbackResult={null} onOperationStart={noop} onOperationEnd={noop}
    onHome={noop} onHistory={noop} /></I18nProvider>);
  await user.click(screen.getByRole("button", { name: "选择迁移包" }));
  expect(api.selectRestoreDestinations).toHaveBeenCalledWith(expect.any(String), true);
  await user.click(screen.getByRole("button", { name: "继续" }));
  await user.click(screen.getByRole("button", { name: "选择已有项目目录" }));
  expect(screen.getByText(destination)).toBeVisible();
  await user.click(screen.getByRole("button", { name: "预览导入内容" }));
  expect(screen.getByText(destination)).toBeVisible();
  expect(screen.queryByText(basePlan.projects_root)).not.toBeInTheDocument();
  expect(screen.queryByText("unused-project-copy-root")).not.toBeInTheDocument();
});
