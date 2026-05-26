import { Button } from "@cap/ui-solid";
import { createWritableMemo } from "@solid-primitives/memo";
import { useMutation } from "@tanstack/solid-query";
import { Show, Suspense } from "solid-js";
import { Input } from "~/routes/editor/ui";
import { generalSettingsStore } from "~/store";
import { commands } from "~/utils/tauri";

type AzureStorageConfig = {
	enabled: boolean;
	accountName: string;
	containerName: string;
	sasToken: string;
};

const DEFAULT_CONFIG: AzureStorageConfig = {
	enabled: false,
	accountName: "",
	containerName: "",
	sasToken: "",
};

export default function AzureConfigPage() {
	const settings = generalSettingsStore.createQuery();

	const currentConfig = (): AzureStorageConfig => {
		const azure = settings.data?.azureStorage;
		if (!azure) return DEFAULT_CONFIG;
		return {
			enabled: azure.enabled,
			accountName: azure.accountName,
			containerName: azure.containerName,
			sasToken: azure.sasToken,
		};
	};

	const [config, setConfig] = createWritableMemo<AzureStorageConfig>(
		() => currentConfig(),
	);

	const sharePreview = () => {
		const c = config();
		if (!c.accountName || !c.containerName) return null;
		return `https://${c.accountName}.blob.core.windows.net/${c.containerName}/<id>.mp4`;
	};

	const saveConfig = useMutation(() => ({
		mutationFn: async (next: AzureStorageConfig) => {
			await generalSettingsStore.set({
				azureStorage: {
					enabled: next.enabled,
					accountName: next.accountName.trim(),
					containerName: next.containerName.trim(),
					sasToken: next.sasToken.trim(),
				},
			});
		},
		onSuccess: async () => {
			await commands.globalMessageDialog("Azure configuration saved.");
		},
	}));

	const clearConfig = useMutation(() => ({
		mutationFn: async () => {
			await generalSettingsStore.set({
				azureStorage: { ...DEFAULT_CONFIG },
			});
			setConfig(DEFAULT_CONFIG);
		},
		onSuccess: async () => {
			await commands.globalMessageDialog("Azure configuration cleared.");
		},
	}));

	const testConnection = useMutation(() => ({
		mutationFn: async (next: AzureStorageConfig) => {
			await generalSettingsStore.set({
				azureStorage: {
					enabled: next.enabled,
					accountName: next.accountName.trim(),
					containerName: next.containerName.trim(),
					sasToken: next.sasToken.trim(),
				},
			});
			await commands.azureTestConnection();
		},
		onSuccess: async () => {
			await commands.globalMessageDialog(
				"Azure connection test passed — Cap can write to your container.",
			);
		},
		onError: async (err) => {
			await commands.globalMessageDialog(
				`Azure connection test failed: ${err instanceof Error ? err.message : String(err)}`,
			);
		},
	}));

	const renderInput = (
		label: string,
		key: keyof AzureStorageConfig,
		placeholder: string,
		type: "text" | "password" = "text",
	) => (
		<div class="space-y-2">
			<label class="text-[13px] text-gray-12">{label}</label>
			<Input
				class="!bg-gray-3"
				type={type}
				value={(config()[key] as string) ?? ""}
				onInput={(e: InputEvent & { currentTarget: HTMLInputElement }) =>
					setConfig({
						...config(),
						[key]: e.currentTarget.value,
					})
				}
				placeholder={placeholder}
				autocomplete="off"
				autocapitalize="off"
				autocorrect="off"
				spellcheck={false}
			/>
		</div>
	);

	return (
		<div class="flex flex-col p-4 h-full">
			<div class="rounded-xl border bg-gray-2 border-gray-4 custom-scroll">
				<div class="flex-1">
					<Suspense
						fallback={
							<div class="flex justify-center items-center w-full h-screen">
								<IconCapLogo class="animate-spin size-16" />
							</div>
						}
					>
						<div class="p-4 space-y-4 animate-in fade-in">
							<div class="pb-4 border-b border-gray-3">
								<p class="text-sm text-gray-11">
									Upload recordings directly to your own Azure Blob Storage
									container. Share links use the public blob URL — no sign-in
									required. Configure a container with anonymous blob-level
									read access, then paste a container-scoped SAS token with
									write permission below.
								</p>
							</div>

							<div class="flex justify-between items-center px-3 py-2 rounded-lg border bg-gray-3 border-gray-4">
								<div class="flex flex-col">
									<span class="text-[13px] text-gray-12 font-medium">
										Use Azure for new recordings
									</span>
									<span class="text-[12px] text-gray-11">
										When on, the Share button bypasses cap.so and uploads to
										your Azure container.
									</span>
								</div>
								<input
									type="checkbox"
									checked={config().enabled}
									onChange={(e) =>
										setConfig({
											...config(),
											enabled: e.currentTarget.checked,
										})
									}
									class="w-5 h-5 accent-blue-9"
								/>
							</div>

							{renderInput(
								"Storage Account Name",
								"accountName",
								"e.g. caprecordings",
							)}
							{renderInput(
								"Container Name",
								"containerName",
								"e.g. cap-videos",
							)}
							{renderInput(
								"Container SAS Token",
								"sasToken",
								"sv=2022-...&sig=...",
								"password",
							)}

							<Show when={sharePreview()}>
								{(preview) => (
									<div class="p-3 rounded-lg border bg-gray-3 border-gray-4">
										<p class="text-[12px] text-gray-11 mb-1">
											Share links will look like:
										</p>
										<code class="text-[12px] text-gray-12 break-all">
											{preview()}
										</code>
									</div>
								)}
							</Show>
						</div>
					</Suspense>
				</div>
			</div>
			<div class="flex-shrink-0 mt-5">
				<fieldset
					class="flex justify-between items-center"
					disabled={
						saveConfig.isPending ||
						clearConfig.isPending ||
						testConnection.isPending
					}
				>
					<div class="flex gap-2">
						<Show when={settings.data?.azureStorage?.accountName}>
							<Button
								variant="destructive"
								onClick={() => clearConfig.mutate()}
							>
								{clearConfig.isPending ? "Clearing..." : "Clear"}
							</Button>
						</Show>
						<Button
							variant="gray"
							onClick={() => testConnection.mutate(config())}
						>
							{testConnection.isPending
								? "Testing..."
								: "Test Connection"}
						</Button>
					</div>
					<Button
						class="min-w-[72px]"
						variant="primary"
						onClick={() => saveConfig.mutate(config())}
					>
						{saveConfig.isPending ? "Saving..." : "Save"}
					</Button>
				</fieldset>
			</div>
		</div>
	);
}
