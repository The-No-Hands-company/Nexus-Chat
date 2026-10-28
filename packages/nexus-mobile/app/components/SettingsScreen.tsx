/**
 * SettingsScreen.tsx - Comprehensive settings (feature parity with desktop)
 */
import React, { useState, useEffect } from "react";
import {
  View, Text, FlatList, Pressable, StyleSheet, TextInput,
  SafeAreaView, Switch, Alert, Modal, ActivityIndicator, ScrollView
} from "react-native";
import { useRouter } from "expo-router";
import { store, useStore, api } from "../lib/store";

function nextOption(current: string, options: string[]): string {
  if (!options.length) return current;
  const index = options.indexOf(current);
  return options[(index + 1) % options.length];
}

interface SettingSection {
  title: string;
  items: SettingItem[];
}

interface SettingBaseItem {
  id: string;
  label: string;
  description?: string;
}

interface ToggleSettingItem extends SettingBaseItem {
  type: "toggle";
  value: boolean;
  onChange?: (val: boolean) => void;
}

interface SelectSettingItem extends SettingBaseItem {
  type: "select";
  value: string;
  options: string[];
  onChange: (val: string) => void;
}

interface TextSettingItem extends SettingBaseItem {
  type: "text";
  value?: string | null;
  onPress?: () => void;
  onChange?: (val: string) => void;
}

interface ButtonSettingItem extends SettingBaseItem {
  type: "button";
  value?: string;
  onPress: () => void;
}

interface DangerSettingItem extends SettingBaseItem {
  type: "danger";
  onPress: () => void;
}

type SettingItem = ToggleSettingItem | SelectSettingItem | TextSettingItem | ButtonSettingItem | DangerSettingItem;

export default function SettingsScreen() {
  useStore();
  const router = useRouter();
  const [activeModal, setActiveModal] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [sessions, setSessions] = useState<any[]>([]);
  const [devices, setDevices] = useState<any[]>([]);

  const user = store.session?.user;

  useEffect(() => {
    loadSessions();
    loadDevices();
  }, []);

  async function loadSessions() {
    try {
      const data = await store.api.getSessions();
      setSessions(data);
    } catch (e) {
      console.error("Failed to load sessions:", e);
    }
  }

  async function loadDevices() {
    try {
      const data = await api.listDevices();
      setDevices(data);
    } catch (e) {
      console.error("Failed to load devices:", e);
    }
  }

  async function handleLogout() {
    Alert.alert(
      "Logout",
      "Are you sure you want to log out?",
      [
        { text: "Cancel", style: "cancel" },
        {
          text: "Logout",
          style: "destructive",
          onPress: async () => {
            await store.logout();
            router.replace("/(auth)/login");
          }
        }
      ]
    );
  }

  async function handleChangePassword(current: string, newPass: string) {
    setLoading(true);
    try {
      await api.changePassword(current, newPass);
      Alert.alert("Success", "Password changed successfully");
      setActiveModal(null);
    } catch (e: any) {
      Alert.alert("Error", e.message);
    } finally {
      setLoading(false);
    }
  }

  async function handleRevokeSession(sessionId: string) {
    try {
      await store.api.revokeSession(sessionId);
      loadSessions();
    } catch (e: any) {
      Alert.alert("Error", e.message);
    }
  }

  async function handleRevokeDevice(deviceId: string) {
    try {
      await api.deleteDevice(deviceId);
      loadDevices();
    } catch (e: any) {
      Alert.alert("Error", e.message);
    }
  }

  async function handleDeleteAccount(password: string) {
    setLoading(true);
    try {
      await api.deleteAccount(password);
      await store.logout();
      router.replace("/(auth)/login");
    } catch (e: any) {
      Alert.alert("Error", e.message);
      setLoading(false);
    }
  }

  const settingsData: SettingSection[] = [
    {
      title: "Account",
      items: [
        {
          id: "username",
          label: "Username",
          type: "text",
          value: user?.username,
          description: "Your unique username on Nexus"
        },
        {
          id: "displayName",
          label: "Display Name",
          type: "text",
          value: user?.displayName,
          onChange: async (val) => {
            await api.updateMe({ displayName: val });
          }
        },
        {
          id: "email",
          label: "Email",
          type: "text",
          value: "(hidden)",
          onPress: () => setActiveModal("email")
        },
        {
          id: "password",
          label: "Change Password",
          type: "button",
          onPress: () => setActiveModal("password")
        }
      ]
    },
    {
      title: "Security",
      items: [
        {
          id: "2fa",
          label: "Two-Factor Authentication",
          type: "toggle",
          value: !!user?.totpEnabled,
          description: "Add an extra layer of security"
        },
        {
          id: "sessions",
          label: "Active Sessions",
          type: "button",
          value: `${sessions.length} active`,
          onPress: () => setActiveModal("sessions")
        },
        {
          id: "devices",
          label: "E2EE Devices",
          type: "button",
          value: `${devices.length} registered`,
          onPress: () => setActiveModal("devices")
        }
      ]
    },
    {
      title: "Privacy",
      items: [
        {
          id: "dm_permissions",
          label: "Who can DM you",
          type: "select",
          value: store.settings.dmPermission || "everyone",
          options: ["everyone", "friends", "nobody"],
          onChange: (val: string) => store.updateSettings({ dmPermission: val as "everyone" | "friends" | "nobody" })
        },
        {
          id: "read_receipts",
          label: "Read Receipts",
          type: "toggle",
          value: store.settings.readReceipts !== false,
          onChange: (val) => store.updateSettings({ readReceipts: val }),
          description: "Show when you've read messages"
        },
        {
          id: "presence",
          label: "Share Presence",
          type: "toggle",
          value: store.settings.sharePresence !== false,
          onChange: (val) => store.updateSettings({ sharePresence: val }),
          description: "Show online status to others"
        }
      ]
    },
    {
      title: "Notifications",
      items: [
        {
          id: "push_enabled",
          label: "Push Notifications",
          type: "toggle",
          value: store.settings.notificationsEnabled,
          onChange: (val) => store.updateSettings({ notificationsEnabled: val })
        },
        {
          id: "mentions_only",
          label: "Mentions Only",
          type: "toggle",
          value: store.settings.mentionsOnly || false,
          onChange: (val) => store.updateSettings({ mentionsOnly: val })
        },
        {
          id: "sounds",
          label: "Sounds",
          type: "toggle",
          value: store.settings.soundsEnabled,
          onChange: (val) => store.updateSettings({ soundsEnabled: val })
        },
        {
          id: "vibration",
          label: "Vibration",
          type: "toggle",
          value: store.settings.vibrationEnabled,
          onChange: (val) => store.updateSettings({ vibrationEnabled: val })
        }
      ]
    },
    {
      title: "Appearance",
      items: [
        {
          id: "theme",
          label: "Theme",
          type: "select",
          value: store.settings.theme,
          options: ["dark", "light", "system"],
          onChange: (val) => store.updateSettings({ theme: val })
        },
        {
          id: "font_size",
          label: "Font Size",
          type: "select",
          value: store.settings.fontSize.toString(),
          options: ["12", "14", "16", "18", "20"],
          onChange: (val) => store.updateSettings({ fontSize: parseInt(val) })
        },
        {
          id: "compact",
          label: "Compact Mode",
          type: "toggle",
          value: store.settings.compactMode,
          onChange: (val) => store.updateSettings({ compactMode: val })
        },
        {
          id: "reduced_motion",
          label: "Reduced Motion",
          type: "toggle",
          value: store.settings.reducedMotion,
          onChange: (val) => store.updateSettings({ reducedMotion: val })
        }
      ]
    },
    {
      title: "Danger Zone",
      items: [
        {
          id: "logout",
          label: "Log Out",
          type: "danger",
          onPress: handleLogout
        },
        {
          id: "delete_account",
          label: "Delete Account",
          type: "danger",
          onPress: () => setActiveModal("delete")
        }
      ]
    }
  ];

  function renderSettingItem(item: SettingItem) {
    switch (item.type) {
      case "toggle":
        return (
          <View style={styles.settingRow}>
            <View style={styles.settingInfo}>
              <Text style={styles.settingLabel}>{item.label}</Text>
              {item.description && <Text style={styles.settingDesc}>{item.description}</Text>}
            </View>
            <Switch
              value={item.value}
              onValueChange={item.onChange}
              trackColor={{ false: "#0f2040", true: "#27c9a5" }}
              thumbColor={item.value ? "#d6f4ff" : "#7890b0"}
            />
          </View>
        );

      case "select":
        return (
          <Pressable
            style={styles.settingRow}
            onPress={() => {
              if (!item.options?.length || !item.onChange) return;
              const current = String(item.value ?? "");
              item.onChange(nextOption(current, item.options));
            }}
          >
            <View style={styles.settingInfo}>
              <Text style={styles.settingLabel}>{item.label}</Text>
            </View>
            <Text style={styles.settingValue}>{item.value}</Text>
          </Pressable>
        );

      case "button":
      case "text":
        return (
          <Pressable style={styles.settingRow} onPress={item.onPress}>
            <View style={styles.settingInfo}>
              <Text style={styles.settingLabel}>{item.label}</Text>
              {item.description && <Text style={styles.settingDesc}>{item.description}</Text>}
            </View>
            <Text style={styles.settingValue} numberOfLines={1}>
              {item.value || "Tap to edit"}
            </Text>
          </Pressable>
        );

      case "danger":
        return (
          <Pressable style={styles.dangerRow} onPress={item.onPress}>
            <Text style={styles.dangerLabel}>{item.label}</Text>
          </Pressable>
        );

      default:
        return null;
    }
  }

  return (
    <SafeAreaView style={styles.container}>
      <View style={styles.header}>
        <Text style={styles.headerTitle}>Settings</Text>
      </View>

      <FlatList
        data={settingsData}
        keyExtractor={section => section.title}
        renderItem={({ item: section }) => (
          <View style={styles.section}>
            <Text style={styles.sectionTitle}>{section.title}</Text>
            {section.items.map(item => (
              <View key={item.id}>
                {renderSettingItem(item)}
              </View>
            ))}
          </View>
        )}
        contentContainerStyle={styles.list}
      />

      {/* Password Change Modal */}
      <Modal visible={activeModal === "password"} animationType="slide" transparent>
        <PasswordChangeModal 
          onClose={() => setActiveModal(null)}
          onSubmit={handleChangePassword}
          loading={loading}
        />
      </Modal>

      {/* Sessions Modal */}
      <Modal visible={activeModal === "sessions"} animationType="slide" transparent>
        <SessionsModal
          sessions={sessions}
          onClose={() => setActiveModal(null)}
          onRevoke={handleRevokeSession}
        />
      </Modal>

      {/* Devices Modal */}
      <Modal visible={activeModal === "devices"} animationType="slide" transparent>
        <DevicesModal
          devices={devices}
          onClose={() => setActiveModal(null)}
          onRevoke={handleRevokeDevice}
        />
      </Modal>

      {/* Delete Account Modal */}
      <Modal visible={activeModal === "delete"} animationType="slide" transparent>
        <DeleteAccountModal
          onClose={() => setActiveModal(null)}
          onSubmit={handleDeleteAccount}
          loading={loading}
        />
      </Modal>
    </SafeAreaView>
  );
}

// Sub-modals
function PasswordChangeModal({ onClose, onSubmit, loading }: any) {
  const [current, setCurrent] = useState("");
  const [newPass, setNewPass] = useState("");
  const [confirm, setConfirm] = useState("");

  return (
    <View style={styles.modalOverlay}>
      <View style={styles.modal}>
        <Text style={styles.modalTitle}>Change Password</Text>
        <TextInput
          style={styles.input}
          placeholder="Current password"
          placeholderTextColor="#7890b0"
          value={current}
          onChangeText={setCurrent}
          secureTextEntry
        />
        <TextInput
          style={styles.input}
          placeholder="New password"
          placeholderTextColor="#7890b0"
          value={newPass}
          onChangeText={setNewPass}
          secureTextEntry
        />
        <TextInput
          style={styles.input}
          placeholder="Confirm new password"
          placeholderTextColor="#7890b0"
          value={confirm}
          onChangeText={setConfirm}
          secureTextEntry
        />
        {newPass && confirm && newPass !== confirm && (
          <Text style={styles.errorText}>Passwords don't match</Text>
        )}
        <View style={styles.modalActions}>
          <Pressable onPress={onClose} style={styles.cancelBtn}>
            <Text style={styles.cancelBtnText}>Cancel</Text>
          </Pressable>
          <Pressable 
            onPress={() => onSubmit(current, newPass)} 
            style={[styles.confirmBtn, loading && styles.disabledBtn]}
            disabled={loading || !current || !newPass || newPass !== confirm}
          >
            {loading ? <ActivityIndicator size="small" color="#062020" /> : <Text style={styles.confirmBtnText}>Change</Text>}
          </Pressable>
        </View>
      </View>
    </View>
  );
}

function SessionsModal({ sessions, onClose, onRevoke }: any) {
  return (
    <View style={styles.modalOverlay}>
      <View style={styles.modal}>
        <Text style={styles.modalTitle}>Active Sessions</Text>
        <ScrollView style={styles.scrollList}>
          {sessions.map((session: any) => (
            <View key={session.id} style={styles.sessionRow}>
              <View style={styles.sessionInfo}>
                <Text style={styles.sessionDevice}>{session.deviceName || "Unknown Device"}</Text>
                <Text style={styles.sessionMeta}>{session.ip} • {session.lastActive}</Text>
                {session.isCurrent && <Text style={styles.currentBadge}>Current</Text>}
              </View>
              {!session.isCurrent && (
                <Pressable onPress={() => onRevoke(session.id)} style={styles.revokeBtn}>
                  <Text style={styles.revokeBtnText}>Revoke</Text>
                </Pressable>
              )}
            </View>
          ))}
        </ScrollView>
        <Pressable onPress={onClose} style={styles.closeBtn}>
          <Text style={styles.closeBtnText}>Close</Text>
        </Pressable>
      </View>
    </View>
  );
}

function DevicesModal({ devices, onClose, onRevoke }: any) {
  return (
    <View style={styles.modalOverlay}>
      <View style={styles.modal}>
        <Text style={styles.modalTitle}>E2EE Devices</Text>
        <ScrollView style={styles.scrollList}>
          {devices.map((device: any) => (
            <View key={device.id} style={styles.deviceRow}>
              <View style={styles.deviceInfo}>
                <Text style={styles.deviceName}>{device.name || `Device ${device.id.slice(0, 8)}`}</Text>
                <Text style={styles.deviceMeta}>Last active: {device.lastActive}</Text>
              </View>
              <Pressable onPress={() => onRevoke(device.id)} style={styles.revokeBtn}>
                <Text style={styles.revokeBtnText}>Remove</Text>
              </Pressable>
            </View>
          ))}
        </ScrollView>
        <Pressable onPress={onClose} style={styles.closeBtn}>
          <Text style={styles.closeBtnText}>Close</Text>
        </Pressable>
      </View>
    </View>
  );
}

function DeleteAccountModal({ onClose, onSubmit, loading }: any) {
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");

  return (
    <View style={styles.modalOverlay}>
      <View style={styles.modal}>
        <Text style={styles.modalTitleDanger}>Delete Account</Text>
        <Text style={styles.warningText}>
          This will permanently delete your account and all data. This action cannot be undone.
        </Text>
        <TextInput
          style={styles.input}
          placeholder="Enter your password"
          placeholderTextColor="#7890b0"
          value={password}
          onChangeText={setPassword}
          secureTextEntry
        />
        <TextInput
          style={styles.input}
          placeholder="Type 'DELETE' to confirm"
          placeholderTextColor="#7890b0"
          value={confirm}
          onChangeText={setConfirm}
          autoCapitalize="characters"
        />
        <View style={styles.modalActions}>
          <Pressable onPress={onClose} style={styles.cancelBtn}>
            <Text style={styles.cancelBtnText}>Cancel</Text>
          </Pressable>
          <Pressable 
            onPress={() => onSubmit(password)} 
            style={[styles.dangerBtn, loading && styles.disabledBtn]}
            disabled={loading || !password || confirm !== "DELETE"}
          >
            {loading ? <ActivityIndicator size="small" color="#fff" /> : <Text style={styles.dangerBtnText}>Delete Forever</Text>}
          </Pressable>
        </View>
      </View>
    </View>
  );
}

const styles = StyleSheet.create({
  container: { flex: 1, backgroundColor: "#030a14" },
  header: { padding: 16, borderBottomWidth: 1, borderBottomColor: "#0f2040" },
  headerTitle: { color: "#d6f4ff", fontSize: 20, fontWeight: "700" },
  list: { paddingBottom: 32 },
  section: { marginTop: 24 },
  sectionTitle: { color: "#7890b0", fontSize: 13, fontWeight: "700", textTransform: "uppercase", letterSpacing: 0.5, paddingHorizontal: 16, marginBottom: 8 },
  settingRow: { flexDirection: "row", alignItems: "center", justifyContent: "space-between", paddingVertical: 14, paddingHorizontal: 16, backgroundColor: "#0b1a30", borderBottomWidth: 1, borderBottomColor: "#071327" },
  settingInfo: { flex: 1, marginRight: 16 },
  settingLabel: { color: "#d6f4ff", fontSize: 16 },
  settingDesc: { color: "#7890b0", fontSize: 13, marginTop: 2 },
  settingValue: { color: "#9ec5d5", fontSize: 15 },
  dangerRow: { paddingVertical: 14, paddingHorizontal: 16, backgroundColor: "#ed42451a", borderBottomWidth: 1, borderBottomColor: "#071327" },
  dangerLabel: { color: "#ed4245", fontSize: 16, fontWeight: "600" },
  modalOverlay: { flex: 1, backgroundColor: "rgba(0,0,0,0.8)", justifyContent: "center", padding: 20 },
  modal: { backgroundColor: "#0b1a30", borderRadius: 16, padding: 20, maxHeight: "80%" },
  modalTitle: { color: "#d6f4ff", fontSize: 20, fontWeight: "700", marginBottom: 16 },
  modalTitleDanger: { color: "#ed4245", fontSize: 20, fontWeight: "700", marginBottom: 16 },
  input: { backgroundColor: "#112544", color: "#d6f4ff", borderRadius: 10, padding: 14, fontSize: 16, marginBottom: 12, borderWidth: 1, borderColor: "#1e3a5f" },
  errorText: { color: "#ed4245", fontSize: 13, marginBottom: 12 },
  warningText: { color: "#ed4245", fontSize: 14, marginBottom: 16, lineHeight: 20 },
  modalActions: { flexDirection: "row", justifyContent: "flex-end", gap: 12, marginTop: 8 },
  cancelBtn: { paddingHorizontal: 16, paddingVertical: 10 },
  cancelBtnText: { color: "#9ec5d5", fontSize: 16 },
  confirmBtn: { backgroundColor: "#27c9a5", borderRadius: 8, paddingHorizontal: 16, paddingVertical: 10 },
  confirmBtnText: { color: "#062020", fontWeight: "700", fontSize: 16 },
  dangerBtn: { backgroundColor: "#ed4245", borderRadius: 8, paddingHorizontal: 16, paddingVertical: 10 },
  dangerBtnText: { color: "#fff", fontWeight: "700", fontSize: 16 },
  disabledBtn: { opacity: 0.5 },
  scrollList: { maxHeight: 300 },
  sessionRow: { flexDirection: "row", alignItems: "center", justifyContent: "space-between", paddingVertical: 12, borderBottomWidth: 1, borderBottomColor: "#112544" },
  sessionInfo: { flex: 1 },
  sessionDevice: { color: "#d6f4ff", fontSize: 15 },
  sessionMeta: { color: "#7890b0", fontSize: 13, marginTop: 2 },
  currentBadge: { color: "#27c9a5", fontSize: 12, fontWeight: "700", marginTop: 4 },
  deviceRow: { flexDirection: "row", alignItems: "center", justifyContent: "space-between", paddingVertical: 12, borderBottomWidth: 1, borderBottomColor: "#112544" },
  deviceInfo: { flex: 1 },
  deviceName: { color: "#d6f4ff", fontSize: 15 },
  deviceMeta: { color: "#7890b0", fontSize: 13, marginTop: 2 },
  revokeBtn: { backgroundColor: "#ed4245", paddingHorizontal: 12, paddingVertical: 6, borderRadius: 6 },
  revokeBtnText: { color: "#fff", fontSize: 13, fontWeight: "600" },
  closeBtn: { backgroundColor: "#112544", paddingVertical: 12, borderRadius: 8, marginTop: 16, alignItems: "center" },
  closeBtnText: { color: "#d6f4ff", fontSize: 16 },
});
