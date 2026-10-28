/**
 * (tabs)/index.tsx - Server list
 */
import React, { useEffect, useState } from "react";
import {
  View, Text, FlatList, Pressable, StyleSheet,
  SafeAreaView, TextInput, Modal, ActivityIndicator,
  Alert,
} from "react-native";
import { useRouter } from "expo-router";
import { store, useStore } from "../lib/store";

function ServerIcon({ name, icon }: { name: string; icon?: string }) {
  if (icon) {
    return <View style={styles.serverIconImg} />;
  }
  return (
    <View style={styles.serverIcon}>
      <Text style={styles.serverIconText}>{name[0]?.toUpperCase() ?? "#"}</Text>
    </View>
  );
}

export default function ServersScreen() {
  useStore();
  const router = useRouter();
  const [creating, setCreating] = useState(false);
  const [newName, setNewName] = useState("");
  const [joining, setJoining] = useState(false);
  const [joinCode, setJoinCode] = useState("");

  useEffect(() => {
    // store already loaded servers via loadInitialData
  }, []);

  async function handleSelectServer(server: typeof store.servers[0]) {
    await store.selectServer(server.id);
    router.push(`/server/${server.id}`);
  }

  async function handleCreateServer() {
    if (!newName.trim()) return;
    setCreating(true);
    try {
      const server = await store.createServer(newName.trim());
      await store.selectServer(server.id);
      router.push(`/server/${server.id}`);
    } catch (e: any) {
      Alert.alert("Error", e.message);
    } finally {
      setCreating(false); setNewName("");
    }
  }

  async function handleJoinServer() {
    if (!joinCode.trim()) return;
    setJoining(true);
    try {
      await store.api.joinServer(joinCode.trim());
      // Reload servers
      const servers = await store.api.getServers();
      store.servers = servers;
    } catch (e: any) {
      Alert.alert("Error", e.message);
    } finally {
      setJoining(false); setJoinCode("");
    }
  }

  const renderServer = ({ item }: { item: typeof store.servers[0] }) => (
    <Pressable onPress={() => handleSelectServer(item)} style={styles.serverRow}>
      <ServerIcon name={item.name} icon={item.icon} />
      <View style={styles.serverInfo}>
        <Text style={styles.serverName}>{item.name}</Text>
        {item.memberCount != null && (
          <Text style={styles.serverMeta}>{item.memberCount} members</Text>
        )}
      </View>
    </Pressable>
  );

  return (
    <SafeAreaView style={styles.container}>
      <View style={styles.header}>
        <Text style={styles.headerTitle}>Servers</Text>
        <Pressable onPress={() => setJoining(true)} style={styles.headerBtn}>
          <Text style={styles.headerBtnText}>Join</Text>
        </Pressable>
        <Pressable onPress={() => setCreating(true)} style={styles.headerBtn}>
          <Text style={styles.headerBtnText}>+</Text>
        </Pressable>
      </View>

      <FlatList
        data={store.servers}
        keyExtractor={item => item.id}
        renderItem={renderServer}
        contentContainerStyle={styles.list}
        ListEmptyComponent={
          <View style={styles.empty}>
            <Text style={styles.emptyText}>No servers yet</Text>
            <Text style={styles.emptySubtext}>Create or join a server to get started</Text>
          </View>
        }
      />

      {/* Create Server Modal */}
      <Modal visible={creating} animationType="slide" transparent>
        <View style={styles.modalOverlay}>
          <View style={styles.modal}>
            <Text style={styles.modalTitle}>Create Server</Text>
            <TextInput
              style={styles.input}
              placeholder="Server name"
              placeholderTextColor="#7890b0"
              value={newName}
              onChangeText={setNewName}
            />
            <View style={styles.modalActions}>
              <Pressable onPress={() => { setCreating(false); setNewName(""); }} style={styles.cancelBtn}>
                <Text style={styles.cancelBtnText}>Cancel</Text>
              </Pressable>
              <Pressable onPress={handleCreateServer} style={styles.confirmBtn} disabled={creating}>
                {creating ? <ActivityIndicator color="#062020" size="small" /> : <Text style={styles.confirmBtnText}>Create</Text>}
              </Pressable>
            </View>
          </View>
        </View>
      </Modal>

      {/* Join Server Modal */}
      <Modal visible={joining} animationType="slide" transparent>
        <View style={styles.modalOverlay}>
          <View style={styles.modal}>
            <Text style={styles.modalTitle}>Join Server</Text>
            <TextInput
              style={styles.input}
              placeholder="Invite code"
              placeholderTextColor="#7890b0"
              value={joinCode}
              onChangeText={setJoinCode}
              autoCapitalize="none"
            />
            <View style={styles.modalActions}>
              <Pressable onPress={() => { setJoining(false); setJoinCode(""); }} style={styles.cancelBtn}>
                <Text style={styles.cancelBtnText}>Cancel</Text>
              </Pressable>
              <Pressable onPress={handleJoinServer} style={styles.confirmBtn} disabled={joining}>
                {joining ? <ActivityIndicator color="#062020" size="small" /> : <Text style={styles.confirmBtnText}>Join</Text>}
              </Pressable>
            </View>
          </View>
        </View>
      </Modal>
    </SafeAreaView>
  );
}

const styles = StyleSheet.create({
  container: { flex: 1, backgroundColor: "#030a14" },
  header: {
    flexDirection: "row", alignItems: "center", padding: 12,
    backgroundColor: "#071327", borderBottomWidth: 1, borderBottomColor: "#0f2040",
  },
  headerTitle: { flex: 1, fontSize: 18, fontWeight: "700", color: "#d6f4ff" },
  headerBtn: { paddingHorizontal: 12, paddingVertical: 6 },
  headerBtnText: { color: "#27c9a5", fontWeight: "700", fontSize: 16 },
  list: { padding: 8 },
  serverRow: { flexDirection: "row", alignItems: "center", padding: 12, borderRadius: 10, marginBottom: 4 },
  serverIcon: {
    width: 48, height: 48, borderRadius: 24, backgroundColor: "#0b1a30",
    justifyContent: "center", alignItems: "center", marginRight: 12,
  },
  serverIconImg: { width: 48, height: 48, borderRadius: 24, backgroundColor: "#1e3a5f", marginRight: 12 },
  serverIconText: { color: "#27c9a5", fontSize: 20, fontWeight: "700" },
  serverInfo: { flex: 1 },
  serverName: { color: "#d6f4ff", fontSize: 16, fontWeight: "600" },
  serverMeta: { color: "#7890b0", fontSize: 12, marginTop: 2 },
  empty: { alignItems: "center", paddingTop: 60 },
  emptyText: { color: "#9ec5d5", fontSize: 18, fontWeight: "600" },
  emptySubtext: { color: "#7890b0", fontSize: 14, marginTop: 8 },
  modalOverlay: { flex: 1, backgroundColor: "rgba(0,0,0,0.7)", justifyContent: "center", padding: 20 },
  modal: { backgroundColor: "#0b1a30", borderRadius: 16, padding: 20 },
  modalTitle: { color: "#d6f4ff", fontSize: 20, fontWeight: "700", marginBottom: 16 },
  input: { backgroundColor: "#112544", color: "#d6f4ff", borderRadius: 10, padding: 14, fontSize: 16, borderWidth: 1, borderColor: "#1e3a5f", marginBottom: 12 },
  modalActions: { flexDirection: "row", justifyContent: "flex-end", gap: 8 },
  cancelBtn: { paddingHorizontal: 16, paddingVertical: 10 },
  cancelBtnText: { color: "#9ec5d5", fontWeight: "600" },
  confirmBtn: { backgroundColor: "#27c9a5", borderRadius: 8, paddingHorizontal: 16, paddingVertical: 10, minWidth: 80, alignItems: "center" },
  confirmBtnText: { color: "#062020", fontWeight: "700" },
});
