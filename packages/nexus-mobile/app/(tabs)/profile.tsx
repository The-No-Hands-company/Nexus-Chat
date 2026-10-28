/**
 * (tabs)/profile.tsx — User profile, settings, change-password, logout
 */
import React, { useState } from "react";
import {
  View, Text, Pressable, StyleSheet, SafeAreaView, ScrollView,
  Alert, TextInput, Modal, ActivityIndicator,
} from "react-native";
import { useRouter } from "expo-router";
import { store, useStore } from "../lib/store";
import { api, getServerUrlPlaceholder } from "../lib/api";
import { gateway } from "../lib/gateway";

function StatusDot({ status }: { status?: string }) {
  const color =
    status === "online"  ? "#57f287"
    : status === "idle"  ? "#fee75c"
    : status === "dnd"   ? "#ed4245"
    : "#aaaaaa";
  return <View style={[pd.statusDot, { backgroundColor: color }]} />;
}

const pd = StyleSheet.create({
  statusDot: { width: 12, height: 12, borderRadius: 6, marginRight: 6 },
});

export default function ProfileScreen() {
  useStore();
  const router = useRouter();
  const user = store.session?.user;
  const [showChangePw, setShowChangePw] = useState(false);
  const [currentPw, setCurrentPw] = useState("");
  const [newPw, setNewPw] = useState("");
  const [confirmPw, setConfirmPw] = useState("");
  const [changingPw, setChangingPw] = useState(false);
  const [savingServerUrl, setSavingServerUrl] = useState(false);
  const [serverUrl, setServerUrl] = useState(store.serverUrl);
  const [gatewayStatus, setGatewayStatus] = useState(store.isAuthenticated ? gateway.status : "disconnected");

  React.useEffect(() => {
    const interval = setInterval(() => setGatewayStatus(gateway.status), 2000);
    return () => clearInterval(interval);
  }, []);

  if (!user) {
    return (
      <SafeAreaView style={s.container}>
        <Text style={s.error}>Not signed in</Text>
      </SafeAreaView>
    );
  }

  function handleLogout() {
    Alert.alert("Sign Out", "Are you sure you want to sign out?", [
      { text: "Cancel", style: "cancel" },
      {
        text: "Sign Out",
        style: "destructive",
        onPress: () => {
          store.logout();
          router.replace("/(auth)/login");
        },
      },
    ]);
  }

  async function handleChangePassword() {
    if (!currentPw || !newPw || !confirmPw) {
      Alert.alert("Error", "All fields are required.");
      return;
    }
    if (newPw !== confirmPw) {
      Alert.alert("Error", "New passwords don't match.");
      return;
    }
    if (newPw.length < 8) {
      Alert.alert("Error", "Password must be at least 8 characters.");
      return;
    }
    if (newPw === currentPw) {
      Alert.alert("Error", "New password must differ from the current one.");
      return;
    }
    setChangingPw(true);
    try {
      await api.changePassword(currentPw, newPw);
      Alert.alert("Success", "Password changed. All other sessions have been signed out.");
      setShowChangePw(false);
      setCurrentPw(""); setNewPw(""); setConfirmPw("");
    } catch (e: unknown) {
      Alert.alert("Error", e instanceof Error ? e.message : "Failed to change password.");
    } finally {
      setChangingPw(false);
    }
  }

  async function handleSaveServerUrl() {
    const normalized = serverUrl.trim().replace(/\/$/, "");
    if (!normalized) {
      Alert.alert("Error", "Server URL is required");
      return;
    }
    setSavingServerUrl(true);
    try {
      await store.setServerUrl(normalized);
      gateway.disconnect();
      await store.loadInitialData();
      await gateway.connect();
      setServerUrl(store.serverUrl);
      setGatewayStatus(gateway.status);
      Alert.alert("Saved", "Server URL updated for this device.");
    } catch (e: unknown) {
      Alert.alert("Error", e instanceof Error ? e.message : "Failed to save server URL.");
    } finally {
      setSavingServerUrl(false);
    }
  }

  const gwColor =
    gatewayStatus === "connected" ? "#57f287"
    : gatewayStatus === "connecting" ? "#fee75c"
    : "#ed4245";

  const currentApi = store.serverUrl || getServerUrlPlaceholder();

  return (
    <SafeAreaView style={s.container}>
      <ScrollView contentContainerStyle={s.content}>

        {/* Avatar + name */}
        <View style={s.hero}>
          <View style={s.avatar}>
            <Text style={s.avatarText}>{user.username[0]?.toUpperCase()}</Text>
          </View>
          <Text style={s.displayName}>{user.displayName ?? user.username}</Text>
          <View style={s.statusRow}>
            <StatusDot status={user.presence} />
            <Text style={s.username}>@{user.username}</Text>
          </View>
          {user.bio ? <Text style={s.bio}>{user.bio}</Text> : null}
        </View>

        {/* Account info */}
        <Section title="Account">
          <InfoRow label="Username" value={`@${user.username}`} />
          <InfoRow label="Email" value={user.email ?? "Not set"} />
          <InfoRow
            label="Member since"
            value={user.createdAt ? new Date(user.createdAt).toLocaleDateString() : "—"}
          />
        </Section>

        {/* Status */}
        <Section title="Presence">
          {(["online", "idle", "dnd", "invisible"] as const).map((p) => (
            <Pressable
              key={p}
              style={[s.presenceRow, user.presence === p && s.presenceRowActive]}
              onPress={async () => {
                try {
                  await api.updateMe({ presence: p });
                  store.updatePresence(p);
                } catch {}
              }}
            >
              <StatusDot status={p} />
              <Text style={s.presenceLabel}>{p.charAt(0).toUpperCase() + p.slice(1)}</Text>
              {user.presence === p && <Text style={s.presenceCheck}>✓</Text>}
            </Pressable>
          ))}
        </Section>

        {/* Security */}
        <Section title="Security">
          <Pressable style={s.actionRow} onPress={() => setShowChangePw(true)}>
            <Text style={s.actionLabel}>Change Password</Text>
            <Text style={s.chevron}>›</Text>
          </Pressable>
        </Section>

        {/* Connection */}
        <Section title="Connection">
          <View style={s.serverEditor}>
            <Text style={s.serverEditorLabel}>Server URL</Text>
            <TextInput
              style={s.serverEditorInput}
              value={serverUrl}
              onChangeText={setServerUrl}
              autoCapitalize="none"
              autoCorrect={false}
              keyboardType="url"
              placeholder={getServerUrlPlaceholder()}
              placeholderTextColor="#556680"
            />
            <Pressable style={s.serverSaveBtn} onPress={handleSaveServerUrl} disabled={savingServerUrl}>
              {savingServerUrl ? (
                <ActivityIndicator color="#062020" size="small" />
              ) : (
                <Text style={s.serverSaveText}>Save Server URL</Text>
              )}
            </Pressable>
          </View>
          <InfoRow label="Current API" value={currentApi} />
          <View style={s.row}>
            <Text style={s.rowLabel}>Gateway</Text>
            <View style={[s.statusBadge, { backgroundColor: gwColor + "22" }]}>
              <View style={[s.gwDot, { backgroundColor: gwColor }]} />
              <Text style={[s.gwText, { color: gwColor }]}>{gatewayStatus}</Text>
            </View>
          </View>
          <InfoRow label="Servers joined" value={String(store.servers.length)} />
        </Section>

        {/* App */}
        <Section title="App">
          <Pressable style={s.actionRow} onPress={() => router.push("/settings")}> 
            <Text style={s.actionLabel}>Open Full Settings</Text>
            <Text style={s.chevron}>›</Text>
          </Pressable>
        </Section>

        {/* Danger */}
        <Pressable style={s.logoutBtn} onPress={handleLogout}>
          <Text style={s.logoutText}>Sign Out</Text>
        </Pressable>

        <Text style={s.version}>Nexus Mobile · v0.1.0</Text>
      </ScrollView>

      {/* Change-password modal */}
      <Modal visible={showChangePw} animationType="slide" transparent>
        <View style={s.overlay}>
          <View style={s.modal}>
            <Text style={s.modalTitle}>Change Password</Text>

            <Text style={s.fieldLabel}>Current password</Text>
            <TextInput
              style={s.input}
              value={currentPw}
              onChangeText={setCurrentPw}
              secureTextEntry
              placeholder="••••••••"
              placeholderTextColor="#556680"
            />
            <Text style={s.fieldLabel}>New password</Text>
            <TextInput
              style={s.input}
              value={newPw}
              onChangeText={setNewPw}
              secureTextEntry
              placeholder="Min. 8 characters"
              placeholderTextColor="#556680"
            />
            <Text style={s.fieldLabel}>Confirm new password</Text>
            <TextInput
              style={s.input}
              value={confirmPw}
              onChangeText={setConfirmPw}
              secureTextEntry
              placeholder="Re-enter new password"
              placeholderTextColor="#556680"
            />

            <View style={s.modalActions}>
              <Pressable
                style={s.cancelBtn}
                onPress={() => { setShowChangePw(false); setCurrentPw(""); setNewPw(""); setConfirmPw(""); }}
              >
                <Text style={s.cancelText}>Cancel</Text>
              </Pressable>
              <Pressable
                style={[s.saveBtn, changingPw && { opacity: 0.6 }]}
                onPress={handleChangePassword}
                disabled={changingPw}
              >
                {changingPw
                  ? <ActivityIndicator color="#062020" size="small" />
                  : <Text style={s.saveText}>Save</Text>}
              </Pressable>
            </View>
          </View>
        </View>
      </Modal>
    </SafeAreaView>
  );
}

// ── Sub-components ─────────────────────────────────────────────────────────────

function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <View style={s.section}>
      <Text style={s.sectionTitle}>{title.toUpperCase()}</Text>
      <View style={s.sectionBody}>{children}</View>
    </View>
  );
}

function InfoRow({ label, value }: { label: string; value: string }) {
  return (
    <View style={s.row}>
      <Text style={s.rowLabel}>{label}</Text>
      <Text style={s.rowValue} numberOfLines={1}>{value}</Text>
    </View>
  );
}

const s = StyleSheet.create({
  container:   { flex: 1, backgroundColor: "#030a14" },
  content:     { paddingBottom: 40 },
  error:       { color: "#ed4245", textAlign: "center", marginTop: 40, fontSize: 16 },
  hero:        { alignItems: "center", paddingTop: 36, paddingBottom: 24 },
  avatar:      { width: 80, height: 80, borderRadius: 40, backgroundColor: "rgba(39,201,165,0.2)", justifyContent: "center", alignItems: "center", marginBottom: 12 },
  avatarText:  { color: "#27c9a5", fontSize: 32, fontWeight: "800" },
  displayName: { color: "#d6f4ff", fontSize: 22, fontWeight: "700" },
  statusRow:   { flexDirection: "row", alignItems: "center", marginTop: 4 },
  username:    { color: "#9ec5d5", fontSize: 14 },
  bio:         { color: "#7890b0", fontSize: 13, textAlign: "center", marginTop: 8, paddingHorizontal: 32 },
  section:     { paddingHorizontal: 16, marginTop: 24 },
  sectionTitle:{ fontSize: 11, fontWeight: "700", color: "#556680", letterSpacing: 1.2, marginBottom: 8 },
  sectionBody: { backgroundColor: "#0b1a30", borderRadius: 12, overflow: "hidden", borderWidth: 1, borderColor: "rgba(255,255,255,0.06)" },
  row:         { flexDirection: "row", alignItems: "center", justifyContent: "space-between", paddingHorizontal: 16, paddingVertical: 13, borderBottomWidth: 1, borderBottomColor: "rgba(255,255,255,0.04)" },
  rowLabel:    { color: "#9ec5d5", fontSize: 15 },
  rowValue:    { color: "#556688", fontSize: 14, maxWidth: "55%", textAlign: "right" },
  actionRow:   { flexDirection: "row", alignItems: "center", justifyContent: "space-between", paddingHorizontal: 16, paddingVertical: 13, borderBottomWidth: 1, borderBottomColor: "rgba(255,255,255,0.04)" },
  actionLabel: { color: "#d6f4ff", fontSize: 15 },
  chevron:     { color: "#556680", fontSize: 20 },
  presenceRow: { flexDirection: "row", alignItems: "center", paddingHorizontal: 16, paddingVertical: 13, borderBottomWidth: 1, borderBottomColor: "rgba(255,255,255,0.04)" },
  presenceRowActive: { backgroundColor: "rgba(39,201,165,0.08)" },
  presenceLabel: { flex: 1, color: "#9ec5d5", fontSize: 15 },
  presenceCheck: { color: "#27c9a5", fontSize: 16, fontWeight: "700" },
  statusBadge: { flexDirection: "row", alignItems: "center", paddingHorizontal: 8, paddingVertical: 4, borderRadius: 12 },
  gwDot:       { width: 7, height: 7, borderRadius: 4, marginRight: 5 },
  gwText:      { fontSize: 13, fontWeight: "600" },
  logoutBtn:   { marginHorizontal: 16, marginTop: 32, backgroundColor: "rgba(237,66,69,0.15)", borderRadius: 12, paddingVertical: 14, alignItems: "center", borderWidth: 1, borderColor: "rgba(237,66,69,0.3)" },
  logoutText:  { color: "#ed4245", fontWeight: "700", fontSize: 15 },
  version:     { color: "#334155", fontSize: 12, textAlign: "center", marginTop: 20 },
  overlay:     { flex: 1, backgroundColor: "rgba(0,0,0,0.75)", justifyContent: "center", padding: 20 },
  modal:       { backgroundColor: "#0b1a30", borderRadius: 16, padding: 20, borderWidth: 1, borderColor: "rgba(255,255,255,0.08)" },
  modalTitle:  { color: "#d6f4ff", fontSize: 20, fontWeight: "700", marginBottom: 16 },
  fieldLabel:  { color: "#7890b0", fontSize: 12, fontWeight: "600", marginBottom: 4, marginTop: 8 },
  input:       { backgroundColor: "#112544", color: "#d6f4ff", borderRadius: 10, paddingHorizontal: 14, paddingVertical: 12, fontSize: 15, borderWidth: 1, borderColor: "rgba(255,255,255,0.08)", marginBottom: 4 },
  modalActions:{ flexDirection: "row", justifyContent: "flex-end", gap: 8, marginTop: 16 },
  cancelBtn:   { paddingHorizontal: 16, paddingVertical: 10 },
  cancelText:  { color: "#9ec5d5", fontWeight: "600", fontSize: 15 },
  saveBtn:     { backgroundColor: "#27c9a5", borderRadius: 10, paddingHorizontal: 20, paddingVertical: 10, minWidth: 80, alignItems: "center" },
  saveText:    { color: "#062020", fontWeight: "700", fontSize: 15 },
  serverEditor: { paddingHorizontal: 16, paddingVertical: 13, borderBottomWidth: 1, borderBottomColor: "rgba(255,255,255,0.04)" },
  serverEditorLabel: { color: "#9ec5d5", fontSize: 12, fontWeight: "700", marginBottom: 8, textTransform: "uppercase", letterSpacing: 0.8 },
  serverEditorInput: { backgroundColor: "#112544", color: "#d6f4ff", borderRadius: 10, paddingHorizontal: 14, paddingVertical: 12, fontSize: 15, borderWidth: 1, borderColor: "rgba(255,255,255,0.08)" },
  serverSaveBtn: { marginTop: 10, backgroundColor: "#27c9a5", borderRadius: 10, paddingVertical: 12, alignItems: "center" },
  serverSaveText: { color: "#062020", fontWeight: "700", fontSize: 14 },
});
