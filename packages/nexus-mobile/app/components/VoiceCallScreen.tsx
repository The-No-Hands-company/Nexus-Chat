/**
 * VoiceCallScreen.tsx - Voice/Video call interface with WebRTC
 */
import React, { useEffect, useState } from "react";
import {
  View, Text, Pressable, StyleSheet, Dimensions, FlatList,
  SafeAreaView, ActivityIndicator
} from "react-native";
import { useRouter } from "expo-router";
import { store, useStore } from "../lib/store";
import { api } from "../lib/api";

const { width: SCREEN_WIDTH } = Dimensions.get("window");

interface Participant {
  userId: string;
  username: string;
  displayName?: string;
  avatar?: string;
  speaking: boolean;
  muted: boolean;
  deafened: boolean;
  videoEnabled?: boolean;
}

export default function VoiceCallScreen() {
  useStore();
  const router = useRouter();
  const [participants, setParticipants] = useState<Participant[]>([]);
  const [connected, setConnected] = useState(false);
  const [connecting, setConnecting] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [layout, setLayout] = useState<"grid" | "speaker">("grid");

  const channelId = store.voiceJoinedChannelId;
  const channel = store.channels.find(c => c.id === channelId);

  useEffect(() => {
    if (!channelId) {
      router.back();
      return;
    }

    // Connect to voice
    connectToVoice();

    // Poll participants
    const interval = setInterval(fetchParticipants, 2000);

    return () => {
      clearInterval(interval);
    };
  }, [channelId]);

  async function connectToVoice() {
    try {
      setConnecting(true);
      await store.joinVoice(channelId!);
      setConnected(true);
      setConnecting(false);
      fetchParticipants();
    } catch (e: any) {
      setError(e.message);
      setConnecting(false);
    }
  }

  async function fetchParticipants() {
    if (!channelId) return;
    try {
      const state = await api.getVoiceState(channelId);
      setParticipants(state.participants || []);
    } catch (e) {
      console.error("Failed to fetch participants:", e);
    }
  }

  async function handleLeave() {
    await store.leaveVoice();
    router.back();
  }

  async function toggleMute() {
    await store.toggleMute();
    setParticipants(prev => prev.map(p => 
      p.userId === store.session?.user.id 
        ? { ...p, muted: !p.muted }
        : p
    ));
  }

  async function toggleDeafen() {
    await store.toggleDeafen();
    setParticipants(prev => prev.map(p => 
      p.userId === store.session?.user.id 
        ? { ...p, deafened: !p.deafened }
        : p
    ));
  }

  function renderParticipant({ item }: { item: Participant }) {
    const isMe = item.userId === store.session?.user.id;
    const isSpeaking = item.speaking && !item.muted;
    
    return (
      <View style={[
        styles.participant,
        layout === "speaker" && styles.participantSpeaker,
        isSpeaking && styles.participantSpeaking
      ]}>
        <View style={styles.avatar}>
          <Text style={styles.avatarText}>
            {(item.displayName || item.username)[0]?.toUpperCase()}
          </Text>
        </View>
        <Text style={styles.participantName} numberOfLines={1}>
          {item.displayName || item.username}
          {isMe ? " (You)" : ""}
        </Text>
        <View style={styles.indicators}>
          {item.muted && <Text style={styles.indicator}>🎤❌</Text>}
          {item.deafened && <Text style={styles.indicator}>🎧❌</Text>}
          {isSpeaking && <View style={styles.speakingIndicator} />}
        </View>
      </View>
    );
  }

  if (connecting) {
    return (
      <SafeAreaView style={styles.container}>
        <View style={styles.center}>
          <ActivityIndicator size="large" color="#27c9a5" />
          <Text style={styles.connectingText}>Connecting to voice...</Text>
        </View>
      </SafeAreaView>
    );
  }

  if (error) {
    return (
      <SafeAreaView style={styles.container}>
        <View style={styles.center}>
          <Text style={styles.errorText}>{error}</Text>
          <Pressable onPress={connectToVoice} style={styles.retryBtn}>
            <Text style={styles.retryBtnText}>Retry</Text>
          </Pressable>
          <Pressable onPress={() => router.back()} style={styles.cancelBtn}>
            <Text style={styles.cancelBtnText}>Cancel</Text>
          </Pressable>
        </View>
      </SafeAreaView>
    );
  }

  return (
    <SafeAreaView style={styles.container}>
      {/* Header */}
      <View style={styles.header}>
        <Pressable onPress={handleLeave} style={styles.leaveBtn}>
          <Text style={styles.leaveBtnText}>Leave Call</Text>
        </Pressable>
        <Text style={styles.headerTitle}>{channel?.name || "Voice"}</Text>
        <View style={styles.layoutToggle}>
          <Pressable onPress={() => setLayout("grid")} style={[styles.toggleBtn, layout === "grid" && styles.toggleActive]}>
            <Text style={styles.toggleText}>Grid</Text>
          </Pressable>
          <Pressable onPress={() => setLayout("speaker")} style={[styles.toggleBtn, layout === "speaker" && styles.toggleActive]}>
            <Text style={styles.toggleText}>Focus</Text>
          </Pressable>
        </View>
      </View>

      {/* Participants */}
      <FlatList
        data={participants}
        keyExtractor={item => item.userId}
        renderItem={renderParticipant}
        numColumns={layout === "grid" ? 2 : 1}
        contentContainerStyle={styles.participantsList}
      />

      {/* Controls */}
      <View style={styles.controls}>
        <Pressable 
          onPress={toggleMute} 
          style={[styles.controlBtn, store.voiceMuted && styles.controlBtnActive]}
        >
          <Text style={styles.controlIcon}>{store.voiceMuted ? "🎤❌" : "🎤"}</Text>
          <Text style={styles.controlLabel}>{store.voiceMuted ? "Unmute" : "Mute"}</Text>
        </Pressable>

        <Pressable 
          onPress={toggleDeafen}
          style={[styles.controlBtn, store.voiceDeafened && styles.controlBtnActive]}
        >
          <Text style={styles.controlIcon}>{store.voiceDeafened ? "🎧❌" : "🎧"}</Text>
          <Text style={styles.controlLabel}>{store.voiceDeafened ? "Undeafen" : "Deafen"}</Text>
        </Pressable>

        <Pressable onPress={handleLeave} style={styles.hangupBtn}>
          <Text style={styles.hangupIcon}>📞</Text>
          <Text style={styles.hangupLabel}>End</Text>
        </Pressable>
      </View>

      {/* Stats */}
      <View style={styles.stats}>
        <Text style={styles.statsText}>{participants.length} participant{participants.length !== 1 ? "s" : ""}</Text>
        {connected && <Text style={styles.connectedBadge}>Connected</Text>}
      </View>
    </SafeAreaView>
  );
}

const styles = StyleSheet.create({
  container: { flex: 1, backgroundColor: "#030a14" },
  center: { flex: 1, justifyContent: "center", alignItems: "center", padding: 20 },
  connectingText: { color: "#9ec5d5", fontSize: 16, marginTop: 16 },
  errorText: { color: "#ed4245", fontSize: 16, textAlign: "center", marginBottom: 20 },
  retryBtn: { backgroundColor: "#27c9a5", paddingHorizontal: 24, paddingVertical: 12, borderRadius: 8, marginBottom: 12 },
  retryBtnText: { color: "#062020", fontWeight: "700", fontSize: 16 },
  cancelBtn: { paddingHorizontal: 24, paddingVertical: 12 },
  cancelBtnText: { color: "#9ec5d5", fontSize: 16 },
  header: { flexDirection: "row", alignItems: "center", padding: 16, borderBottomWidth: 1, borderBottomColor: "#0f2040" },
  leaveBtn: { backgroundColor: "#ed4245", paddingHorizontal: 12, paddingVertical: 8, borderRadius: 8 },
  leaveBtnText: { color: "#fff", fontWeight: "700", fontSize: 14 },
  headerTitle: { flex: 1, textAlign: "center", color: "#d6f4ff", fontSize: 18, fontWeight: "700" },
  layoutToggle: { flexDirection: "row", backgroundColor: "#0b1a30", borderRadius: 8 },
  toggleBtn: { paddingHorizontal: 12, paddingVertical: 6 },
  toggleActive: { backgroundColor: "#1e3a5f", borderRadius: 8 },
  toggleText: { color: "#9ec5d5", fontSize: 12 },
  participantsList: { padding: 16, gap: 12 },
  participant: { 
    flex: 1, aspectRatio: 1, backgroundColor: "#0b1a30", borderRadius: 16, 
    justifyContent: "center", alignItems: "center", margin: 6,
    borderWidth: 2, borderColor: "transparent"
  },
  participantSpeaker: { aspectRatio: 16/9, width: SCREEN_WIDTH - 32 },
  participantSpeaking: { borderColor: "#27c9a5" },
  avatar: { width: 64, height: 64, borderRadius: 32, backgroundColor: "#1e3a5f", justifyContent: "center", alignItems: "center", marginBottom: 12 },
  avatarText: { color: "#d6f4ff", fontSize: 28, fontWeight: "700" },
  participantName: { color: "#d6f4ff", fontSize: 16, fontWeight: "600", maxWidth: "80%" },
  indicators: { flexDirection: "row", marginTop: 8, gap: 8 },
  indicator: { fontSize: 16 },
  speakingIndicator: { width: 8, height: 8, borderRadius: 4, backgroundColor: "#27c9a5" },
  controls: { flexDirection: "row", justifyContent: "center", gap: 24, paddingVertical: 24, borderTopWidth: 1, borderTopColor: "#0f2040" },
  controlBtn: { alignItems: "center", backgroundColor: "#0b1a30", padding: 16, borderRadius: 16, minWidth: 80 },
  controlBtnActive: { backgroundColor: "#ed4245" },
  controlIcon: { fontSize: 24, marginBottom: 4 },
  controlLabel: { color: "#9ec5d5", fontSize: 12, fontWeight: "600" },
  hangupBtn: { alignItems: "center", backgroundColor: "#ed4245", padding: 16, borderRadius: 16, minWidth: 80 },
  hangupIcon: { fontSize: 24, marginBottom: 4 },
  hangupLabel: { color: "#fff", fontSize: 12, fontWeight: "700" },
  stats: { flexDirection: "row", justifyContent: "space-between", alignItems: "center", paddingHorizontal: 16, paddingBottom: 16 },
  statsText: { color: "#7890b0", fontSize: 14 },
  connectedBadge: { backgroundColor: "#27c9a5", color: "#062020", fontSize: 12, fontWeight: "700", paddingHorizontal: 8, paddingVertical: 4, borderRadius: 4, overflow: "hidden" },
});
