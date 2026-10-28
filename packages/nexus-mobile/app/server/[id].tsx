/**
 * server/[id].tsx - Server detail with channel list
 */
import React, { useEffect, useState } from "react";
import {
  View, Text, FlatList, Pressable, StyleSheet,
  SafeAreaView, TextInput, Modal, Alert,
  ScrollView,
} from "react-native";
import { useLocalSearchParams, useRouter } from "expo-router";
import { store, useStore } from "../lib/store";

const CHANNEL_ICONS: Record<string, string> = {
  text: "#", voice: "V", announcement: "!", forum: "F", stage: "S", default: "#",
};

export default function ServerScreen() {
  useStore();
  const { id } = useLocalSearchParams<{ id: string }>();
  const router = useRouter();
  const [creatingChannel, setCreatingChannel] = useState(false);
  const [newChannelName, setNewChannelName] = useState("");
  const [newChannelType, setNewChannelType] = useState<"text" | "voice">("text");

  useEffect(() => {
    if (id && store.activeServerId !== id) {
      store.selectServer(id);
    }
  }, [id]);

  const server = store.servers.find(s => s.id === id);
  const channels = store.channels;

  // Group channels by category (type)
  const textChannels = channels.filter(c => c.kind === "text" || c.kind === "announcement" || c.kind === "forum");
  const voiceChannels = channels.filter(c => c.kind === "voice" || c.kind === "stage");

  async function handleSelectChannel(channel: typeof channels[0]) {
    await store.selectChannel(channel.id);
    router.push("/channel/" + channel.id);
  }

  async function handleCreateChannel() {
    if (!newChannelName.trim() || !id) return;
    try {
      const channel = await store.createChannel(id, newChannelName.trim(), newChannelType);
      await store.selectChannel(channel.id);
      router.push("/channel/" + channel.id);
    } catch (e: any) {
      Alert.alert("Error", e.message);
    } finally {
      setCreatingChannel(false); setNewChannelName("");
    }
  }

  return (
    <SafeAreaView style={styles.container}>
      <View style={styles.header}>
        <Pressable onPress={() => router.back()} style={styles.backBtn}>
          <Text style={styles.backBtnText}>Back</Text>
        </Pressable>
        <Text style={styles.serverName} numberOfLines={1}>{server?.name ?? "Server"}</Text>
        <Pressable onPress={() => setCreatingChannel(true)} style={styles.addBtn}>
          <Text style={styles.addBtnText}>+</Text>
        </Pressable>
      </View>

      {server?.description ? (
        <Text style={styles.description}>{server.description}</Text>
      ) : null}

      <FlatList
        data={[...textChannels, ...voiceChannels]}
        keyExtractor={item => item.id}
        renderItem={({ item }) => {
          const icon = CHANNEL_ICONS[item.kind] ?? CHANNEL_ICONS.default;
          const isUnread = !!store.unreadChannels[item.id];
          const isVoice = item.kind === "voice" || item.kind === "stage";
          return (
            <Pressable
              onPress={() => {
                if (isVoice) {
                  store.joinVoice(item.id).then(() => router.push(`/voice/${item.id}`));
                  return;
                }
                handleSelectChannel(item);
              }}
              style={[styles.channelRow, isUnread && styles.channelRowUnread]}
            >
              <Text style={[styles.channelIcon, item.kind === "voice" && styles.voiceIcon]}>{icon}</Text>
              <Text style={[styles.channelName, isUnread && styles.channelNameUnread]}>{item.name}</Text>
              {item.isE2ee ? <Text style={styles.e2eeIcon}>*</Text> : null}
            </Pressable>
          );
        }}
        contentContainerStyle={styles.list}
        ListEmptyComponent={
          <View style={styles.empty}>
            <Text style={styles.emptyText}>No channels yet</Text>
            <Text style={styles.emptySubtext}>Create your first channel to get started</Text>
          </View>
        }
      />

      <Modal visible={creatingChannel} animationType="slide" transparent>
        <View style={styles.modalOverlay}>
          <View style={styles.modal}>
            <Text style={styles.modalTitle}>Create Channel</Text>
            <TextInput
              style={styles.input}
              placeholder="Channel name"
              placeholderTextColor="#7890b0"
              value={newChannelName}
              onChangeText={setNewChannelName}
            />
            <View style={styles.typeRow}>
              <Pressable
                onPress={() => setNewChannelType("text")}
                style={[styles.typeBtn, newChannelType === "text" && styles.typeBtnActive]}
              >
                <Text style={styles.typeBtnText}># Text</Text>
              </Pressable>
              <Pressable
                onPress={() => setNewChannelType("voice")}
                style={[styles.typeBtn, newChannelType === "voice" && styles.typeBtnActive]}
              >
                <Text style={styles.typeBtnText}>V Voice</Text>
              </Pressable>
            </View>
            <View style={styles.modalActions}>
              <Pressable onPress={() => { setCreatingChannel(false); setNewChannelName(""); }} style={styles.cancelBtn}>
                <Text style={styles.cancelBtnText}>Cancel</Text>
              </Pressable>
              <Pressable onPress={handleCreateChannel} style={styles.confirmBtn}>
                <Text style={styles.confirmBtnText}>Create</Text>
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
  header: { flexDirection: "row", alignItems: "center", padding: 12, backgroundColor: "#071327", borderBottomWidth: 1, borderBottomColor: "#0f2040" },
  backBtn: { paddingRight: 12 },
  backBtnText: { color: "#27c9a5", fontSize: 15, fontWeight: "600" },
  serverName: { flex: 1, fontSize: 18, fontWeight: "700", color: "#d6f4ff" },
  addBtn: { paddingHorizontal: 12, paddingVertical: 6 },
  addBtnText: { color: "#27c9a5", fontSize: 20, fontWeight: "700" },
  description: { color: "#9ec5d5", fontSize: 13, paddingHorizontal: 16, paddingVertical: 8, backgroundColor: "#071327" },
  list: { padding: 8 },
  channelRow: { flexDirection: "row", alignItems: "center", padding: 12, borderRadius: 8, marginBottom: 2 },
  channelRowUnread: { backgroundColor: "#0f1f35" },
  channelIcon: { color: "#7890b0", fontSize: 18, marginRight: 10, width: 24 },
  voiceIcon: { color: "#27c9a5" },
  channelName: { flex: 1, color: "#d6f4ff", fontSize: 16 },
  channelNameUnread: { color: "#fff", fontWeight: "700" },
  e2eeIcon: { color: "#27c9a5", fontSize: 14 },
  empty: { alignItems: "center", paddingTop: 60 },
  emptyText: { color: "#9ec5d5", fontSize: 18 },
  emptySubtext: { color: "#7890b0", fontSize: 14, marginTop: 8 },
  modalOverlay: { flex: 1, backgroundColor: "rgba(0,0,0,0.7)", justifyContent: "center", padding: 20 },
  modal: { backgroundColor: "#0b1a30", borderRadius: 16, padding: 20 },
  modalTitle: { color: "#d6f4ff", fontSize: 20, fontWeight: "700", marginBottom: 16 },
  input: { backgroundColor: "#112544", color: "#d6f4ff", borderRadius: 10, padding: 14, fontSize: 16, borderWidth: 1, borderColor: "#1e3a5f", marginBottom: 12 },
  typeRow: { flexDirection: "row", gap: 8, marginBottom: 16 },
  typeBtn: { flex: 1, padding: 10, borderRadius: 8, backgroundColor: "#112544", alignItems: "center" },
  typeBtnActive: { backgroundColor: "#27c9a5" },
  typeBtnText: { color: "#d6f4ff", fontWeight: "600" },
  modalActions: { flexDirection: "row", justifyContent: "flex-end", gap: 8 },
  cancelBtn: { paddingHorizontal: 16, paddingVertical: 10 },
  cancelBtnText: { color: "#9ec5d5", fontWeight: "600" },
  confirmBtn: { backgroundColor: "#27c9a5", borderRadius: 8, paddingHorizontal: 16, paddingVertical: 10 },
  confirmBtnText: { color: "#062020", fontWeight: "700" },
});
